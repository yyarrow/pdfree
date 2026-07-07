use clap::{Parser, Subcommand};
use lopdf::Document;

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
            let doc = Document::load(&input)?;
            let runs = pdfree_core::extract_runs(&doc)?;
            let pages = doc.get_pages().len();
            Ok(serde_json::to_string(&serde_json::json!({
                "pages": pages,
                "runs": runs,
            }))?)
        }
        Cmd::Replace {
            input,
            output,
            page,
            find,
            with_text,
        } => {
            let mut doc = Document::load(&input)?;
            let report = pdfree_core::replace_text(&mut doc, page, &find, &with_text)?;
            doc.save(&output)?;
            Ok(serde_json::to_string(&report)?)
        }
    }
}
