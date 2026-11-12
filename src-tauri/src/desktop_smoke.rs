//! Opt-in native smoke test. Only the network measurements are substituted.

use crate::{
    engine::{EngineOutcome, EngineUpdate, PreparedScan, UpdateCallback},
    model::{ExclusionCode, IpResult, ScanProgress},
    network::{InterfaceKind, NetworkPath},
    stability,
};
use std::{collections::BTreeMap, net::Ipv4Addr, path::PathBuf};
use tauri::{plugin, AppHandle, Builder, Context, Listener, Manager, Wry};
use tokio_util::sync::CancellationToken;

const FINISHED: &str = "ip-picker-smoke-finished";

fn close_smoke_window(app: &AppHandle, phase: &str) -> tauri::Result<()> {
    if cfg!(target_os = "macos") && phase == "exercise" {
        #[cfg(target_os = "macos")]
        return crate::macos_menu::exercise_quit_menu(app);
    }
    app.get_webview_window("main")
        .expect("missing smoke window")
        .close()
}

pub(crate) fn data_dir() -> Result<PathBuf, String> {
    let path = std::env::var_os("IP_PICKER_SMOKE_DATA_DIR")
        .map(PathBuf::from)
        .ok_or("Run the desktop smoke via npm run test:desktop-smoke")?;
    if !path.is_absolute()
        || !path.is_dir()
        || !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("ip-picker-smoke-"))
    {
        return Err("The desktop smoke requires its own temporary data directory".into());
    }
    Ok(path)
}

pub(crate) fn configure(
    builder: Builder<Wry>,
    mut context: Context<Wry>,
) -> (Builder<Wry>, Context<Wry>) {
    let path = data_dir().expect("missing desktop smoke data directory");
    let phase = std::env::var("IP_PICKER_SMOKE_PHASE").expect("missing desktop smoke phase");
    assert!(matches!(phase.as_str(), "exercise" | "reopen"));
    context.config_mut().identifier = "com.ippicker.desktop-smoke".into();
    for window in &mut context.config_mut().app.windows {
        window.incognito = true;
        window.data_directory = Some(path.join("webview"));
    }

    let script = format!(
        "(() => {{ const phase = '{}'; {} }})();",
        phase,
        include_str!("desktop_smoke.js")
    );
    let builder = builder.plugin(
        plugin::Builder::<Wry>::new("desktop-smoke")
            .js_init_script(script)
            .setup(move |app, _| {
                let app_handle = app.clone();
                app.once(FINISHED, move |event| {
                    match serde_json::from_str::<Option<String>>(event.payload()) {
                        Ok(None) => {
                            println!("desktop smoke {phase} passed");
                            if let Err(error) = close_smoke_window(&app_handle, &phase) {
                                eprintln!("desktop smoke could not request close: {error}");
                                app_handle.exit(1);
                            }
                        }
                        result => {
                            eprintln!("desktop smoke {phase} failed: {result:?}");
                            app_handle.exit(1);
                        }
                    }
                });
                Ok(())
            })
            .build(),
    );
    (builder, context)
}

pub(crate) async fn run_prepared_scan(
    _prepared: PreparedScan,
    cancellation: CancellationToken,
    callback: UpdateCallback,
) -> Result<EngineOutcome, String> {
    let mut result = IpResult::new(Ipv4Addr::LOCALHOST);
    result.tcp_ms = 20.0;
    result.region_code = "HK".into();
    result.colo = "HKG".into();
    result.observed_colos.push("HKG".into());
    result.attempts = 1;
    result.success_count = 1;
    let mut tcp = stability::summarize(100, 230.0, vec![Some(412.0), None]);
    tcp.failure_details.push(stability::TcpProbeFailure {
        attempt: 2,
        failure: stability::TcpConnectFailure {
            kind: stability::TcpConnectFailureKind::Timeout,
            elapsed_ms: Some(1501.2),
            message: None,
        },
    });
    result.tcp_stability = Some(tcp);
    let mut progress = ScanProgress {
        stage: "stabilizing".into(),
        completed: 2,
        total: 100,
        discovered_by_region: BTreeMap::from([("HK".into(), 1)]),
        message: "桌面冒烟测试样本".into(),
        network_path: Some(NetworkPath {
            sample_ip: Ipv4Addr::LOCALHOST.to_string(),
            interface_name: "en0".into(),
            interface_kind: Some(InterfaceKind::Wifi),
            tunnel_interface: false,
            captured_at: chrono::Utc::now().to_rfc3339(),
        }),
        ..Default::default()
    };
    callback(EngineUpdate {
        progress: progress.clone(),
        results: vec![result.clone()],
    });
    cancellation.cancelled().await;
    result.exclude(ExclusionCode::Cancelled, "测试已停止，已保留样本");
    progress.stage = "cancelled".into();
    Ok(EngineOutcome {
        progress,
        results: vec![result],
        cancelled: true,
    })
}
