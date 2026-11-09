//! Offscreen PNG rendering of the real application for visual review.
//!
//! Runs the actual `eframe::App` through egui_kittest and renders each scene
//! with wgpu, preferring a CPU adapter (Mesa lavapipe on Linux/WSL). No window,
//! display server or ADB is used. This is review tooling, not a pixel test:
//!
//! ```sh
//! cargo test --features ui-snapshot --bin ciallogcat ui_snapshots -- --ignored --nocapture
//! ```
//!
//! PNGs are written to `target/ui-snapshots/` (override with
//! `CIALLOGCAT_SNAPSHOT_DIR`). `CIALLOGCAT_SNAPSHOT_ADAPTER=gpu` prefers a
//! hardware adapter; the default `cpu` gives machine-independent pixels. On
//! WSL the GPU is only reachable through Mesa's GL-on-D3D12 driver
//! (`GALLIUM_DRIVER=d3d12 LD_LIBRARY_PATH=/usr/lib/wsl/lib`), whose readback
//! currently times out in wgpu; use the CPU default there.
use super::*;
use egui_kittest::Harness;
use std::path::PathBuf;

fn output_dir() -> PathBuf {
    std::env::var_os("CIALLOGCAT_SNAPSHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/ui-snapshots"))
}

const SAMPLE: &[(Level, &str, &str, &str)] = &[
    (
        Level::Info,
        "com.example.shop",
        "ActivityManager",
        "Start proc 23456:com.example.shop/u0a312 for activity {com.example.shop/.MainActivity}",
    ),
    (
        Level::Debug,
        "com.example.shop",
        "OkHttp",
        "--> POST https://api.example.com/v2/checkout (412-byte body)",
    ),
    (
        Level::Verbose,
        "com.example.shop",
        "Choreographer",
        "Skipped 3 frames! The application may be doing too much work on its main thread.",
    ),
    (
        Level::Info,
        "com.example.chat",
        "ChatSync",
        "同步完成：12 条新消息，耗时 184 ms",
    ),
    (
        Level::Warn,
        "com.example.shop",
        "PaymentClient",
        "Retrying payment authorization (attempt 2/3) after timeout",
    ),
    (
        Level::Debug,
        "system_server",
        "WindowManager",
        "Changing focus from Window{8f1c u0 StatusBar} to Window{2ab9 u0 com.example.shop/.CheckoutActivity}",
    ),
    (
        Level::Error,
        "com.example.shop",
        "PaymentClient",
        "Payment authorization failed: java.net.SocketTimeoutException: timeout",
    ),
    (
        Level::Info,
        "com.example.chat",
        "FirebaseMessaging",
        "Received message id=0:1730000000%abcdef",
    ),
    (
        Level::Fatal,
        "com.example.shop",
        "AndroidRuntime",
        "FATAL EXCEPTION: main\nProcess: com.example.shop, PID: 23456\njava.lang.IllegalStateException: Checkout session expired\n\tat com.example.shop.checkout.CheckoutViewModel.submit(CheckoutViewModel.kt:118)\n\tat com.example.shop.checkout.CheckoutFragment.onClick(CheckoutFragment.kt:64)",
    ),
    (
        Level::Warn,
        "system_server",
        "ActivityTaskManager",
        "Force finishing activity com.example.shop/.CheckoutActivity",
    ),
];

fn populated_app(cc: &eframe::CreationContext<'_>, dark: bool, rows: usize) -> CiallogcatApp {
    let config = AppConfig {
        dark,
        ..Default::default()
    };
    let mut app = CiallogcatApp::with_config(&cc.egui_ctx, config, None);
    for i in 0..rows {
        let (level, process, tag, message) = SAMPLE[i % SAMPLE.len()];
        let pid = if process == "system_server" {
            1543
        } else {
            23456 + (i % 3) as u32
        };
        let mut entry = LogEntry::new(
            format!(
                "11-09 14:{:02}:{:02}.{:03}",
                (i / 60) % 60,
                i % 60,
                (i * 37) % 1000
            ),
            pid,
            pid + (i % 7) as u32,
            tag,
            level,
            message,
        );
        entry.process = Some(Arc::from(process));
        app.append_entry(entry);
    }
    app.follow_logs = false;
    app
}

fn wgpu_setup() -> eframe::egui_wgpu::WgpuSetup {
    use eframe::wgpu;
    let prefer_gpu = std::env::var("CIALLOGCAT_SNAPSHOT_ADAPTER").as_deref() == Ok("gpu");
    let mut setup = eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    setup.instance_descriptor.backends = wgpu::Backends::VULKAN | wgpu::Backends::GL;
    setup.native_adapter_selector = Some(Arc::new(move |adapters, _surface| {
        let rank = |adapter: &&wgpu::Adapter| {
            let cpu = adapter.get_info().device_type == wgpu::DeviceType::Cpu;
            cpu == prefer_gpu
        };
        let chosen = adapters
            .iter()
            .min_by_key(rank)
            .cloned()
            .ok_or_else(|| "No wgpu adapter found".to_owned())?;
        let info = chosen.get_info();
        eprintln!(
            "selected: {:?} {:?} {}",
            info.backend, info.device_type, info.name
        );
        Ok(chosen)
    }));
    eframe::egui_wgpu::WgpuSetup::CreateNew(setup)
}

struct Scene {
    name: &'static str,
    size: (f32, f32),
    dark: bool,
    rows: usize,
    setup: fn(&mut CiallogcatApp),
}

fn settle(harness: &mut Harness<'_, CiallogcatApp>) {
    // The filter worker runs on another thread; step until its result lands.
    for _ in 0..200 {
        harness.step();
        let app = harness.state();
        if !app.filter_pending && app.filter_dirty_since.is_none() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    harness.run_steps(4);
}

fn render(scene: &Scene) {
    let dark = scene.dark;
    let rows = scene.rows;
    let mut harness = Harness::builder()
        .with_size(egui::vec2(scene.size.0, scene.size.1))
        // kittest applies its own theme after the app is built; match the scene.
        .with_theme(if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        })
        .wgpu_setup(wgpu_setup())
        .build_eframe(|cc| populated_app(cc, dark, rows));
    settle(&mut harness);
    (scene.setup)(harness.state_mut());
    harness.state_mut().queue_filter();
    settle(&mut harness);
    let image = harness.render().expect("offscreen render");
    let dir = output_dir();
    std::fs::create_dir_all(&dir).expect("snapshot dir");
    let path = dir.join(format!("{}.png", scene.name));
    image.save(&path).expect("write png");
    println!("{}", path.display());
}

#[test]
#[ignore = "visual review tooling; needs a wgpu adapter"]
fn ui_snapshots() {
    let dir = output_dir();
    std::fs::create_dir_all(dir.join("config")).expect("config dir");
    // Never touch the user's real settings.
    unsafe { std::env::set_var("CIALLOGCAT_CONFIG_DIR", dir.join("config")) };

    let scenes = [
        Scene {
            name: "empty-dark",
            size: (1280.0, 820.0),
            dark: true,
            rows: 0,
            setup: |_| {},
        },
        Scene {
            name: "logs-dark",
            size: (1280.0, 820.0),
            dark: true,
            rows: 400,
            setup: |_| {},
        },
        Scene {
            name: "logs-light",
            size: (1280.0, 820.0),
            dark: false,
            rows: 400,
            setup: |_| {},
        },
        Scene {
            name: "details-dark",
            size: (1280.0, 820.0),
            dark: true,
            rows: 400,
            setup: |app| {
                let index = 8;
                app.selection
                    .select(&(0..400).collect::<Vec<_>>(), index, false, false);
                app.selected = Some(index);
                app.scroll_to_selected = true;
            },
        },
        Scene {
            name: "filtered-compact-dark",
            size: (900.0, 560.0),
            dark: true,
            rows: 400,
            setup: |app| {
                app.package = "com.example.shop".to_owned();
                app.query = "payment".to_owned();
                app.min_level = Level::Warn;
            },
        },
        Scene {
            name: "shortcuts-compact-dark",
            size: (900.0, 560.0),
            dark: true,
            rows: 400,
            setup: |app| app.show_shortcuts = true,
        },
        Scene {
            name: "settings-appearance-compact-dark",
            size: (900.0, 560.0),
            dark: true,
            rows: 400,
            setup: |app| app.open_settings(Some(super::settings_ui::Page::Appearance)),
        },
        Scene {
            name: "settings-reading-light",
            size: (1280.0, 820.0),
            dark: false,
            rows: 400,
            setup: |app| app.open_settings(Some(super::settings_ui::Page::Reading)),
        },
        Scene {
            name: "settings-memory-warning-compact-dark",
            size: (900.0, 560.0),
            dark: true,
            rows: 400,
            setup: |app| {
                app.open_settings(Some(super::settings_ui::Page::Memory));
                for _ in 0..144 {
                    app.append_entry(LogEntry::marker("x".repeat(128 * 1024)));
                }
                app.memory_limit_draft = Some(16);
            },
        },
        Scene {
            name: "en-logs-compact-dark",
            size: (900.0, 560.0),
            dark: true,
            rows: 400,
            setup: |app| {
                app.language = Language::En;
                app.follow_logs = true;
            },
        },
        Scene {
            name: "en-logs-wide-light",
            size: (1280.0, 820.0),
            dark: false,
            rows: 400,
            setup: |app| {
                app.language = Language::En;
                app.follow_logs = true;
                let index = 8;
                app.selection
                    .select(&(0..400).collect::<Vec<_>>(), index, false, false);
                app.selected = Some(index);
            },
        },
        Scene {
            name: "en-settings-appearance-compact-dark",
            size: (900.0, 560.0),
            dark: true,
            rows: 400,
            setup: |app| {
                app.language = Language::En;
                app.open_settings(Some(super::settings_ui::Page::Appearance));
            },
        },
    ];
    for scene in &scenes {
        render(scene);
    }
}
