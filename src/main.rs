//! signal-tray: a tiny, keyboard-driven Signal client that floats in a
//! screen corner and hides in the tray.
//!
//! The app is single-instance: running `signal-tray toggle` while it is
//! already running forwards the command to the running instance, which is
//! how the global show/hide shortcut works (bind it in your compositor).

mod app;
mod config;
mod demo;
mod model;
mod signal;
mod store;
mod tray;
mod ui;

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gio, glib};

use crate::app::{Controller, Mode};
use crate::config::Config;

const APP_ID: &str = "io.github.elsirion.SignalTray";

#[derive(Clone, Copy, Debug)]
enum Command {
    Toggle,
    Show,
    Hide,
    Quit,
}

impl Command {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "toggle" => Some(Self::Toggle),
            "show" => Some(Self::Show),
            "hide" => Some(Self::Hide),
            "quit" => Some(Self::Quit),
            _ => None,
        }
    }
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // The demo gets its own application id so it can run next to (and never
    // talk to) a real instance. It has to be known before GApplication
    // parses options, hence the manual peek.
    let mode = if std::env::args().any(|a| a == "--demo") {
        Mode::Demo
    } else {
        Mode::Live
    };
    let app_id = match mode {
        Mode::Live => APP_ID.to_owned(),
        Mode::Demo => format!("{APP_ID}.Demo"),
    };

    let app = gtk::Application::builder()
        .application_id(app_id)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    app.add_main_option(
        "demo",
        glib::Char::from(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Show sample data instead of connecting to Signal",
        None,
    );
    app.add_main_option(
        "hidden",
        glib::Char::from(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Start minimized to the tray",
        None,
    );
    app.set_option_context_parameter_string(Some("[toggle|show|hide|quit]"));
    app.set_option_context_summary(Some(
        "Compact Signal client. Commands are sent to the running instance;\n\
         bind `signal-tray toggle` to a key in your compositor.",
    ));

    let controller: Rc<RefCell<Option<Rc<Controller>>>> = Rc::default();
    app.connect_command_line(move |app, cmdline| {
        handle_command_line(app, cmdline, &controller, mode)
    });
    app.run()
}

fn handle_command_line(
    app: &gtk::Application,
    cmdline: &gio::ApplicationCommandLine,
    slot: &RefCell<Option<Rc<Controller>>>,
    mode: Mode,
) -> glib::ExitCode {
    let args: Vec<String> = cmdline
        .arguments()
        .iter()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let command = match args.as_slice() {
        [] => None,
        [arg] => match Command::parse(arg) {
            Some(command) => Some(command),
            None => {
                cmdline.printerr_literal(&format!(
                    "unknown command {arg:?}; expected toggle, show, hide or quit\n"
                ));
                return glib::ExitCode::FAILURE;
            }
        },
        _ => {
            cmdline.printerr_literal("expected at most one command\n");
            return glib::ExitCode::FAILURE;
        }
    };
    let start_hidden = cmdline.options_dict().contains("hidden");

    let existing = slot.borrow().clone();
    let Some(controller) = existing else {
        // First invocation: this process is the primary instance.
        let config_path = Config::default_path();
        let config = match Config::load(&config_path) {
            Ok(config) => config,
            Err(err) => {
                cmdline.printerr_literal(&format!("{err:#}\n"));
                return glib::ExitCode::FAILURE;
            }
        };
        let controller = match Controller::new(app, config, config_path, mode) {
            Ok(controller) => controller,
            Err(err) => {
                cmdline.printerr_literal(&format!("failed to start: {err:#}\n"));
                return glib::ExitCode::FAILURE;
            }
        };
        match command {
            Some(Command::Quit) => app.quit(),
            Some(Command::Hide) => {}
            None if start_hidden || controller.start_hidden() => {}
            None | Some(Command::Show | Command::Toggle) => controller.ui().show(),
        }
        *slot.borrow_mut() = Some(controller);
        return glib::ExitCode::SUCCESS;
    };

    match command {
        None | Some(Command::Show) => controller.ui().show(),
        Some(Command::Toggle) => controller.ui().toggle(),
        Some(Command::Hide) => controller.ui().hide(),
        Some(Command::Quit) => app.quit(),
    }
    glib::ExitCode::SUCCESS
}
