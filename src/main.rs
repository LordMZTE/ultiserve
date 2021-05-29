use std::{
    ffi::OsStr,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
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

    let opt = Opt::from_args_safe()?;

    let mut tera = Tera::default();
    // add templates. we wanna read them at compile time, so we have a single complete binary.
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
    warp::serve(warp::path::full().and_then(move |path| on_get(path, Arc::clone(&tools))))
        .run(addr)
        .await;

    Ok(())
}

/// this is called once we get a request.
async fn on_get(full_path: FullPath, tools: Arc<Tools>) -> Result<Box<dyn Reply>, Rejection> {
    // start off at path to serve (defaults to .)
    let mut path = tools.opt.dir.clone();
    // we don't want to go to root, so we remove the / at the start
    path.push(full_path.as_str().trim_start_matches('/'));
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
                current_dir: full_path.as_str().trim_end_matches('/').to_string(),
                has_parent: full_path.as_str() != "/",
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
                    Ok(file_content) => render_file_to_reply(tools, &path, file_content)
                        .map(|r| Box::new(r) as Box<dyn Reply>),
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
) -> Result<Html<String>, Rejection> {
    let file_ext = path.extension().and_then(OsStr::to_str);
    // don't put html files into the file template, just send them raw.
    if let Some("html") = file_ext {
        return Ok(reply::html(content));
    }

    let mut unsafe_content = false;
    if let Some(syntax) = file_ext.and_then(|e| tools.syntax_set.find_syntax_by_extension(e)) {
        let theme = &tools.theme_set.themes["Dracula"];
        content = highlighted_html_for_string(&content, &tools.syntax_set, syntax, theme);
        unsafe_content = true;
    }

    create_file_reply(tools, path, content, unsafe_content)
}

/// renders the file template, returning a reply or rejection.
fn create_file_reply(
    tools: Arc<Tools>,
    path: &Path,
    content: String,
    unsafe_content: bool,
) -> Result<Html<String>, Rejection> {
    Context::from_serialize(FileContent {
        content,
        unsafe_content,
        file_name: path
            .canonicalize()
            .map(|b| b.to_string_lossy().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string()),
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
}
