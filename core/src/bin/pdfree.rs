use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "pdfree", about = "pdfree PDF engine CLI")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Extract text segments with positions as JSON.
    Extract { input: String },
    /// Print the page's text model (blocks -> lines -> runs) as JSON.
    Model {
        input: String,
        #[arg(long)]
        page: u32,
    },
    /// Replace a run's text via the model (same length only for now).
    ReplaceRun {
        input: String,
        output: String,
        #[arg(long)]
        page: u32,
        #[arg(long)]
        block: usize,
        #[arg(long)]
        line: usize,
        #[arg(long)]
        run: usize,
        #[arg(long = "with")]
        with_text: String,
        #[arg(long)]
        fallback_font: Option<String>,
    },
    /// Replace the first occurrence of a string on a page.
    Replace {
        input: String,
        output: String,
        #[arg(long)]
        page: u32,
        #[arg(long)]
        find: String,
        #[arg(long = "with")]
        with_text: String,
        /// TTF font supplying glyphs the document's fonts lack.
        #[arg(long)]
        fallback_font: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    match run(cli) {
        Ok(json) => println!("{json}"),
        Err(e) => {
            eprintln!("{{\"error\": {}}}", serde_json::json!(e.to_string()));
            std::process::exit(1);
        }
    }
}

fn run(cli: Cli) -> Result<String, Box<dyn std::error::Error>> {
    match cli.cmd {
        Cmd::Extract { input } => {
            let doc = pdfree_core::load_with_salvage(std::path::Path::new(&input))?;
            let runs = pdfree_core::extract_runs(&doc)?;
            let pages = doc.get_pages().len();
            Ok(serde_json::to_string(&serde_json::json!({
                "pages": pages,
                "runs": runs,
            }))?)
        }
        Cmd::Model { input, page } => {
            let doc = pdfree_core::load_with_salvage(std::path::Path::new(&input))?;
            Ok(serde_json::to_string(&pdfree_core::extract_model(&doc, page))?)
        }
        Cmd::ReplaceRun {
            input,
            output,
            page,
            block,
            line,
            run,
            with_text,
            fallback_font,
        } => {
            let ttf = match fallback_font {
                Some(path) => Some(
                    pdfree_core::TtfFont::parse(std::fs::read(path)?)
                        .ok_or("failed to parse fallback font")?,
                ),
                None => None,
            };
            let mut doc = pdfree_core::load_with_salvage(std::path::Path::new(&input))?;
            let report =
                pdfree_core::replace_run_text(&mut doc, page, block, line, run, &with_text, ttf.as_ref())?;
            doc.save(&output)?;
            Ok(serde_json::to_string(&report)?)
        }
        Cmd::Replace {
            input,
            output,
            page,
            find,
            with_text,
            fallback_font,
        } => {
            let ttf = match fallback_font {
                Some(path) => Some(
                    pdfree_core::TtfFont::parse(std::fs::read(path)?)
                        .ok_or("failed to parse fallback font")?,
                ),
                None => None,
            };
            let mut doc = pdfree_core::load_with_salvage(std::path::Path::new(&input))?;
            let report = pdfree_core::replace_text(&mut doc, page, &find, &with_text, ttf.as_ref())?;
            doc.save(&output)?;
            Ok(serde_json::to_string(&report)?)
        }
    }
}
