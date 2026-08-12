// SPDX-License-Identifier: Apache-2.0

//! Standalone shell and fixture child entry used by the upstream acceptance
//! matrix. BowEcho embeds [`crate::StudioApp`] directly and does not call this.

/// Run the standalone Studio command line and return a process exit code.
pub fn run_standalone(args: &[String]) -> i32 {
    if args.get(1).map(String::as_str) == Some("--fixture-run") {
        return fixture_run(&args[2..]);
    }

    let smoke_seconds: Option<f64> = option_value(args, "--smoke-seconds");
    let livefire_mode = args
        .iter()
        .position(|arg| arg == "--livefire")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| crate::livefire::LivefireMode::parse(value));
    let close_after = option_value(args, "--close-after").unwrap_or(40.0);
    let dry_run = args.iter().any(|arg| arg == "--dry-run");
    let livefire_hours = option_value(args, "--livefire-hours").unwrap_or(1.0);

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title(format!("ArWen Studio · {}", crate::app::build_stamp()))
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([1000.0, 640.0]),
        ..Default::default()
    };
    let owned_args = args.to_vec();
    let result = eframe::run_native(
        "ArWen Studio",
        native_options,
        Box::new(move |creation| {
            let mut app = crate::StudioApp::new(&creation.egui_ctx);
            if let Some(seconds) = smoke_seconds {
                app.set_smoke_seconds(seconds);
            }
            if owned_args.iter().any(|arg| arg == "--demo-draft") {
                app.set_demo_draft();
            }
            if owned_args.iter().any(|arg| arg == "--demo-offcentre") {
                app.set_demo_offcentre();
            }
            if owned_args.iter().any(|arg| arg == "--demo-drawroot") {
                app.set_demo_drawroot();
            }
            if owned_args.iter().any(|arg| arg == "--demo-run") {
                app.set_demo_drawroot_run();
            }
            if owned_args.iter().any(|arg| arg == "--demo-carry") {
                app.set_demo_carry();
            }
            if let Some(mode) = livefire_mode {
                let mut driver = crate::livefire::LivefireDriver::new(mode, close_after);
                driver.dry_run = dry_run;
                driver.hours = livefire_hours;
                app.livefire = Some(driver);
            }
            Ok(Box::new(app))
        }),
    );
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("arwen-studio: {error}");
            1
        }
    }
}

fn option_value<T: std::str::FromStr>(args: &[String], name: &str) -> Option<T> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse().ok())
}

/// `arwen-studio --fixture-run <events.jsonl> --run-dir <dir> [--speed N]`
fn fixture_run(args: &[String]) -> i32 {
    let mut events = None;
    let mut run_dir = None;
    let mut speed = 60.0_f64;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--run-dir" => {
                run_dir = args.get(index + 1).cloned();
                index += 2;
            }
            "--speed" => {
                speed = args
                    .get(index + 1)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(60.0);
                index += 2;
            }
            other => {
                if events.is_none() {
                    events = Some(other.to_string());
                }
                index += 1;
            }
        }
    }
    let (Some(events), Some(run_dir)) = (events, run_dir) else {
        eprintln!("usage: arwen-studio --fixture-run <events.jsonl> --run-dir <dir> [--speed N]");
        return 2;
    };
    let options = arwen_proc::replay::ReplayOptions {
        speed,
        max_gap: std::time::Duration::from_secs(2),
    };
    let mut stdout = std::io::stdout();
    match arwen_proc::replay::replay_fixture_run(
        std::path::Path::new(&events),
        std::path::Path::new(&run_dir),
        &mut stdout,
        &options,
    ) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("fixture-run: {error}");
            1
        }
    }
}
