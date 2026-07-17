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
    /// Print the /Info document metadata dictionary as JSON.
    Info { input: String },
    /// Set fields on /Info, leaving unspecified fields untouched.
    SetInfo {
        input: String,
        output: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        author: Option<String>,
        #[arg(long)]
        subject: Option<String>,
        #[arg(long)]
        keywords: Option<String>,
        #[arg(long)]
        creator: Option<String>,
    },
    /// Rotate selected pages by a multiple of 90 degrees (added to current rotation).
    Rotate {
        input: String,
        output: String,
        /// 1-based page spec, e.g. "1-2,4".
        #[arg(long)]
        pages: String,
        /// One of 90, 180, 270, -90.
        #[arg(long, allow_negative_numbers = true)]
        degrees: i64,
    },
    /// Delete selected pages from the document.
    DeletePages {
        input: String,
        output: String,
        /// 1-based page spec, e.g. "1-2,4".
        #[arg(long)]
        pages: String,
    },
    /// Reorder pages to a given permutation.
    Reorder {
        input: String,
        output: String,
        /// 1-based permutation of 1..=N, e.g. "3,1,2".
        #[arg(long)]
        order: String,
    },
    /// Concatenate the pages of several PDFs into one, in argument order.
    Merge {
        output: String,
        #[arg(required = true, num_args = 1..)]
        inputs: Vec<String>,
    },
    /// Write a new PDF containing only the selected pages.
    Split {
        input: String,
        output: String,
        /// 1-based page range spec, e.g. "1-3,5,8-10".
        #[arg(long)]
        pages: String,
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
        Cmd::Info { input } => {
            let doc = pdfree_core::load_with_salvage(std::path::Path::new(&input))?;
            Ok(serde_json::to_string(&pdfree_core::read_info(&doc))?)
        }
        Cmd::SetInfo {
            input,
            output,
            title,
            author,
            subject,
            keywords,
            creator,
        } => {
            let mut fields: Vec<(&str, &str)> = Vec::new();
            if let Some(v) = &title {
                fields.push(("Title", v));
            }
            if let Some(v) = &author {
                fields.push(("Author", v));
            }
            if let Some(v) = &subject {
                fields.push(("Subject", v));
            }
            if let Some(v) = &keywords {
                fields.push(("Keywords", v));
            }
            if let Some(v) = &creator {
                fields.push(("Creator", v));
            }
            let mut doc = pdfree_core::load_with_salvage(std::path::Path::new(&input))?;
            pdfree_core::set_info(&mut doc, &fields)?;
            doc.save(&output)?;
            Ok(serde_json::to_string(&pdfree_core::read_info(&doc))?)
        }
        Cmd::Rotate {
            input,
            output,
            pages,
            degrees,
        } => {
            if ![90, 180, 270, -90].contains(&degrees) {
                return Err("degrees must be one of 90, 180, 270, -90".into());
            }
            let mut doc = pdfree_core::load_with_salvage(std::path::Path::new(&input))?;
            let num_pages = doc.get_pages().len() as u32;
            let page_list = pdfree_core::parse_page_spec(&pages, num_pages)?;
            pdfree_core::rotate_pages(&mut doc, &page_list, degrees)?;
            doc.save(&output)?;
            Ok(serde_json::to_string(&serde_json::json!({
                "pages_rotated": page_list,
                "degrees": degrees,
            }))?)
        }
        Cmd::DeletePages { input, output, pages } => {
            let mut doc = pdfree_core::load_with_salvage(std::path::Path::new(&input))?;
            let num_pages_before = doc.get_pages().len() as u32;
            let page_list = pdfree_core::parse_page_spec(&pages, num_pages_before)?;
            pdfree_core::delete_pages(&mut doc, &page_list)?;
            let num_pages_after = doc.get_pages().len() as u32;
            doc.save(&output)?;
            Ok(serde_json::to_string(&serde_json::json!({
                "pages_deleted": page_list,
                "pages_before": num_pages_before,
                "pages_after": num_pages_after,
            }))?)
        }
        Cmd::Reorder { input, output, order } => {
            let mut doc = pdfree_core::load_with_salvage(std::path::Path::new(&input))?;
            let num_pages = doc.get_pages().len() as u32;
            let order_list = pdfree_core::parse_order_spec(&order, num_pages)?;
            pdfree_core::reorder_pages(&mut doc, &order_list)?;
            doc.save(&output)?;
            Ok(serde_json::to_string(&serde_json::json!({
                "order": order_list,
            }))?)
        }
        Cmd::Merge { output, inputs } => {
            let paths: Vec<std::path::PathBuf> = inputs.iter().map(std::path::PathBuf::from).collect();
            let mut doc = pdfree_core::merge(&paths)?;
            doc.save(&output)?;
            let pages = doc.get_pages().len();
            Ok(serde_json::to_string(&serde_json::json!({ "pages": pages }))?)
        }
        Cmd::Split { input, output, pages } => {
            let doc = pdfree_core::load_with_salvage(std::path::Path::new(&input))?;
            let mut out = pdfree_core::extract_pages(&doc, &pages)?;
            out.save(&output)?;
            let n = out.get_pages().len();
            Ok(serde_json::to_string(&serde_json::json!({ "pages": n }))?)
        }
    }
}
