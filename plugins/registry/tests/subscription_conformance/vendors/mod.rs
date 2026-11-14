mod antigravity;
mod claude;
mod cline;
mod codex;
mod copilot;
mod cursor;
mod devin;
mod factory;
mod gemini;
mod grok;
mod kiro;
mod qoder;
mod workbuddy;
mod zcode;
mod zed;

use crate::matrix::Cell;

pub async fn execute(cell: Cell) -> Result<(), String> {
    match cell.vendor {
        "claude" => claude::run(cell).await,
        "cursor" => cursor::execute(cell).await,
        "devin" => devin::run(cell).await,
        "zed" => zed::run(cell).await,
        "codex" => codex::run(cell).await,
        "copilot" => copilot::run(cell).await,
        "gemini" => gemini::run(cell).await,
        "antigravity" => antigravity::run(cell).await,
        "grok" => grok::run(cell).await,
        "kiro" => kiro::run(cell).await,
        "factory" => factory::run(cell).await,
        "cline" => cline::run(cell).await,
        "qoder" => qoder::run(cell).await,
        "workbuddy" => workbuddy::run(cell).await,
        "zcode" => zcode::run(cell).await,
        _ => Err("unknown matrix vendor".to_owned()),
    }
}
