use comrak::{
    nodes::{AstNode, NodeCodeBlock, NodeHtmlBlock, NodeValue},
    Arena,
    ComrakExtensionOptions,
    ComrakOptions,
    ComrakRenderOptions,
};
use crossterm::{
    execute,
    style::{Colorize, Print, PrintStyledContent},
};
use std::{
    ffi::OsStr,
    io::stdout,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
use structopt::StructOpt;
use syntect::{
    dumps,
    highlighting::ThemeSet,
    html::highlighted_html_for_string,
    parsing::SyntaxSet,
};
use tera::{Context, Tera};
use warp::{
    path::FullPath,
    reject::{self, Reject},
    reply,
    reply::Html,
    Filter,
    Rejection,
    Reply,
};

#[derive(Debug, StructOpt)]
#[structopt(about = "Serve your files over http!")]
struct Opt {
    #[structopt(
        short,
        long,
        help = "The address to bind the server to",
        default_value = "127.0.0.1:8080"
    )]
    addr: SocketAddr,

    #[structopt(help = "The directory to serve", default_value = ".")]
    dir: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // initialize logger for debugging
    env_logger::init();

    let opt = Opt::from_args();

    // show startup message
    execute!(
        stdout(),
        Print("Serving files at "),
        // we always print localhost, no matter the bind address
        PrintStyledContent(format!("http://127.0.0.1:{}\n", &opt.addr.port()).green()),
    )?;

    let mut tera = Tera::default();
    // add templates. we wanna read them at compile time, so we have a single
    // complete binary.
    tera.add_raw_templates(vec![
        (
            "index.html",
            include_str!("../assets/templates/index.html.tera"),
        ),
        (
            "base.html",
            include_str!("../assets/templates/base.html.tera"),
        ),
        (
            "file.html",
            include_str!("../assets/templates/file.html.tera"),
        ),
        (
            "macros.html",
            include_str!("../assets/templates/macros.html.tera"),
        ),
    ])?;

    // previously binary dumped version of the dracula theme for performance
    let theme_set = dumps::from_binary(include_bytes!("../assets/Dracula.themedump"));
    let tools = Arc::new(Tools {
        tera,
        syntax_set: SyntaxSet::load_defaults_newlines(),
        theme_set,
        opt,
    });

    let addr = tools.opt.addr;
    warp::serve(
        warp::path::full()
            .and(warp::query::<GetParams>())
            .and_then(move |path, get_params| on_get_timed(path, get_params, Arc::clone(&tools))),
    )
    .run(addr)
    .await;

    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct GetParams {
    #[serde(default)]
    raw: bool,
}

// calls on_get and prints the time needed to execute it.
async fn on_get_timed(
    full_path: FullPath,
    get_params: GetParams,
    tools: Arc<Tools>,
) -> Result<Box<dyn Reply>, Rejection> {
    let path_str = full_path.as_str().to_string();
    let start_time = Instant::now();
    let reply = on_get(full_path, get_params, Arc::clone(&tools)).await;
    let time_needed = start_time.elapsed();
    println!(
        "Processed request to {} in {}{}",
        path_str.blue(),
        time_needed.as_millis().to_string().red(),
        "ms".red(),
    );

    reply
}

/// this is called once we get a request.
async fn on_get(
    full_path: FullPath,
    get_params: GetParams,
    tools: Arc<Tools>,
) -> Result<Box<dyn Reply>, Rejection> {
    let full_path = full_path.as_str();
    // start off at path to serve (defaults to .)
    let mut path = tools.opt.dir.clone();
    // we don't want to go to root, so we remove the / at the start
    path.push(full_path.trim_start_matches('/'));
    match tokio::fs::read_dir(&path).await {
        // if we have a dir render index page
        Ok(mut dir) => {
            let mut files = vec![];

            // iterate over dir with tokio's async API
            // that also means we can't use iterators :(
            while let Ok(Some(entry)) = dir.next_entry().await {
                let mut name = entry.file_name().to_string_lossy().into_owned();

                let mut is_dir = false;
                if path.join(&name).is_dir() {
                    // add / to end of name to make it clear it's a directory
                    name.push('/');
                    is_dir = true;
                }

                let entry = FileEntry { name, is_dir };

                files.push(entry);
            }

            // sort by name
            // TODO use proper alphabetical sorting
            files.sort_by(|a, b| a.name.cmp(&b.name));

            let content = IndexContent {
                files,
                full_current_dir: path
                    .canonicalize()
                    .map(|s| s.to_string_lossy().into_owned())
                    // insert "<unknown>" in case something goes wrong getting the name of the
                    // current directory
                    .unwrap_or_else(|_| "<unknown>".to_string()),
                current_dir: full_path.trim_end_matches('/').to_string(),
                has_parent: full_path != "/",
            };

            if let Ok(rendered) =
                Context::from_serialize(content).and_then(|c| tools.tera.render("index.html", &c))
            {
                Ok(Box::new(reply::html(rendered)))
            } else {
                // TODO implement reject handlers
                Err(reject::custom(UltiserveReject::RenderFail))
            }
        },
        // if there's no dir use the file template
        _ => {
            if let Ok(bytes) = tokio::fs::read(&path).await {
                // check if we have valid utf8
                match String::from_utf8(bytes.clone()) {
                    // if the file is valid utf8, render the file template
                    Ok(file_content) => {
                        if get_params.raw {
                            Ok(Box::new(file_content))
                        } else {
                            render_file_to_reply(
                                tools,
                                &path,
                                file_content,
                                full_path.trim_end_matches("/"),
                            )
                            .map(|r| Box::new(r) as Box<dyn Reply>)
                        }
                    },
                    // if the file is not utf8, give the client the raw bytes
                    _ => Ok(Box::new(bytes)),
                }
            } else {
                // if there is no file, 404
                Err(reject::reject())
            }
        },
    }
}

/// renders a file using the file template, and turns it into a reply.
fn render_file_to_reply(
    tools: Arc<Tools>,
    path: &Path,
    mut content: String,
    url: &str,
) -> Result<Html<String>, Rejection> {
    let file_ext = path.extension().and_then(OsStr::to_str);
    match file_ext {
        // don't put html files into the file template, just send them raw.
        Some("html") | Some("html5") => Ok(reply::html(content)),
        // render markdown
        Some("md") | Some("markdown") => {
            render_markdown_to_reply(Arc::clone(&tools), path, &content, url)
        },
        ext => {
            let mut unsafe_content = false;

            if let Some(highlighted) =
                ext.and_then(|ext| syntax_highlight_html(Arc::clone(&tools), ext, &content))
            {
                content = highlighted;
                unsafe_content = true;
            }

            create_file_reply(tools, path, content, unsafe_content, url)
        },
    }
}

/// highlights the given string with syntax for the given file extension if it
/// exists, and renders it as html.
fn syntax_highlight_html(tools: Arc<Tools>, file_ext: &str, content: &str) -> Option<String> {
    tools.syntax_set.find_syntax_by_token(file_ext).map(|s| {
        let theme = &tools.theme_set.themes["Dracula"];
        highlighted_html_for_string(content, &tools.syntax_set, s, theme)
    })
}

/// renders a markdown file to a http reply
fn render_markdown_to_reply(
    tools: Arc<Tools>,
    path: &Path,
    content: &str,
    url: &str,
) -> Result<Html<String>, Rejection> {
    let arena = Arena::new();
    let options = ComrakOptions {
        extension: ComrakExtensionOptions {
            strikethrough: true,
            table: true,
            autolink: true,
            tasklist: true,
            header_ids: Some("user-content-".to_string()),
            ..Default::default()
        },
        render: ComrakRenderOptions {
            // needed to render syntax highlighted code blocks
            unsafe_: true,
            ..Default::default()
        },
        parse: Default::default(),
    };

    let document = comrak::parse_document(&arena, content, &options);

    // recursive function to iterate over markdown AST
    fn iter_nodes<'a, F>(node: &'a AstNode<'a>, f: &F)
    where
        F: Fn(&'a AstNode<'a>),
    {
        f(node);
        for c in node.children() {
            iter_nodes(c, f);
        }
    }

    iter_nodes(document, &|node| {
        let mut new_val = None;
        if let NodeValue::CodeBlock(NodeCodeBlock {
            fenced: true,
            info,
            literal,
            ..
        }) = &node.data.borrow().value
        {
            // only continue if info and content are valid utf8
            if let (Ok(info), Ok(literal)) = (
                // clone required to allocate string
                String::from_utf8(info.clone()),
                String::from_utf8(literal.clone()),
            ) {
                if let Some(html) = syntax_highlight_html(Arc::clone(&tools), &info, &literal) {
                    let mut html_block = NodeHtmlBlock::default();
                    html_block.literal = html.into();
                    new_val = Some(NodeValue::HtmlBlock(html_block));
                }
            }
        }

        // we have to do this here, so node isn't borrowed while we swap the value
        if let Some(val) = new_val {
            node.data.borrow_mut().value = val;
        }
    });

    let mut html = vec![];
    comrak::format_html(document, &options, &mut html)
        .map_err(|_| UltiserveReject::MarkdownFail)?;
    let html = String::from_utf8(html).map_err(|_| UltiserveReject::MarkdownFail)?;

    create_file_reply(tools, path, html, true, url)
}

/// renders the file template, returning a reply or rejection.
fn create_file_reply(
    tools: Arc<Tools>,
    path: &Path,
    content: String,
    unsafe_content: bool,
    url: &str,
) -> Result<Html<String>, Rejection> {
    Context::from_serialize(FileContent {
        content,
        unsafe_content,
        file_name: path
            .canonicalize()
            .map(|b| b.to_string_lossy().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string()),
        raw_url: format!("{}?raw=true", url),
    })
    .and_then(|c| tools.tera.render("file.html", &c))
    .map(reply::html)
    .map_err(|_| reject::custom(UltiserveReject::RenderFail))
}

/// a set of tools passed to the request handler
struct Tools {
    /// template engine
    tera: Tera,
    syntax_set: SyntaxSet,
    theme_set: ThemeSet,
    /// command line options
    opt: Opt,
}

#[derive(Debug)]
enum UltiserveReject {
    RenderFail,
    MarkdownFail,
}

impl Reject for UltiserveReject {}

#[derive(Debug, serde::Serialize)]
struct IndexContent {
    files: Vec<FileEntry>,
    full_current_dir: String,
    current_dir: String,
    has_parent: bool,
}

#[derive(Debug, serde::Serialize)]
struct FileEntry {
    name: String,
    is_dir: bool,
}

#[derive(Debug, serde::Serialize)]
struct FileContent {
    content: String,
    unsafe_content: bool,
    file_name: String,
    raw_url: String,
}
