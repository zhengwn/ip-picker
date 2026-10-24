//! Run a deliberately configured scan without starting the desktop window.
//! No scan is started unless a JSON configuration file is provided.

use ip_picker_lib::{
    engine::{run_scan, EngineUpdate},
    model::ScanConfig,
};
use std::{
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    let mut args = std::env::args_os();
    let program = args.next().unwrap_or_else(|| "scan".into());
    let Some(config_path) = args.next() else {
        eprintln!("Usage: {} <config.json>", program.to_string_lossy());
        std::process::exit(2);
    };
    if args.next().is_some() {
        eprintln!("Usage: {} <config.json>", program.to_string_lossy());
        std::process::exit(2);
    }

    if let Err(error) = scan(PathBuf::from(config_path)).await {
        eprintln!("scan: {error}");
        std::process::exit(1);
    }
}

async fn scan(config_path: PathBuf) -> Result<(), String> {
    let contents = std::fs::read(&config_path)
        .map_err(|error| format!("无法读取配置 {}：{error}", config_path.display()))?;
    let config: ScanConfig =
        serde_json::from_slice(&contents).map_err(|error| format!("配置 JSON 无效：{error}"))?;
    config.validate()?;

    let last_progress = Arc::new(Mutex::new((String::new(), usize::MAX)));
    let callback = Arc::new(move |update: EngineUpdate| {
        let progress = update.progress;
        let bucket = if progress.total == 0 {
            0
        } else {
            progress.completed.saturating_mul(10) / progress.total
        };
        if let Ok(mut previous) = last_progress.lock() {
            if previous.0 != progress.stage || previous.1 != bucket {
                eprintln!(
                    "[{}] {}/{} · {}",
                    progress.stage, progress.completed, progress.total, progress.message
                );
                *previous = (progress.stage, bucket);
            }
        }
    });
    let outcome = run_scan(config, CancellationToken::new(), callback).await?;
    let output = serde_json::json!({
        "progress": outcome.progress,
        "results": outcome.results,
    });
    let stdout = io::stdout();
    let mut writer = io::BufWriter::new(stdout.lock());
    serde_json::to_writer_pretty(&mut writer, &output)
        .map_err(|error| format!("无法写出结果 JSON：{error}"))?;
    writeln!(writer).map_err(|error| format!("无法写出结果：{error}"))?;
    writer
        .flush()
        .map_err(|error| format!("无法写出结果：{error}"))
}
