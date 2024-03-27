#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![feature(const_option, let_chains, trait_alias, try_blocks)]

mod backend;
mod frontend;

use crate::backend::config::RawConfig;
use crate::backend::farmer::FarmerAction;
use crate::backend::{wipe, BackendAction, BackendNotification};
use crate::frontend::configuration::{ConfigurationInput, ConfigurationOutput, ConfigurationView};
use crate::frontend::loading::{LoadingInput, LoadingView};
use crate::frontend::new_version::NewVersion;
use crate::frontend::running::{RunningInit, RunningInput, RunningOutput, RunningView};
use clap::Parser;
use duct::cmd;
use file_rotate::compression::Compression;
use file_rotate::suffix::AppendCount;
use file_rotate::{ContentLimit, FileRotate};
use futures::channel::mpsc;
use futures::{select, FutureExt, SinkExt, StreamExt};
use gtk::prelude::*;
use native_dialog::{MessageDialog, MessageType};
use parking_lot::Mutex;
use relm4::prelude::*;
use relm4::{Sender, ShutdownReceiver, RELM_THREADS};
use relm4_icons::icon_name;
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Termination};
use std::sync::Arc;
use std::thread::available_parallelism;
use std::{env, fs, io, process};
use subspace_farmer::utils::{run_future_in_dedicated_thread, AsyncJoinOnDrop};
use subspace_proof_of_space::chia::ChiaTable;
use tracing::{error, info, warn};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

/// Number of log files to keep
const LOG_FILE_LIMIT_COUNT: usize = 5;
/// Size of one log file
const LOG_FILE_LIMIT_SIZE: usize = 1024 * 1024 * 10;
const LOG_READ_BUFFER: usize = 1024 * 1024;
/// If `true`, this means supervisor will not be able to capture logs from child application and logger needs to be in
/// the child process itself, while supervisor will not attempt to read stdout/stderr at all
const WINDOWS_SUBSYSTEM_WINDOWS: bool = cfg!(all(windows, not(debug_assertions)));

#[derive(Debug, Copy, Clone)]
enum AppStatusCode {
    Exit,
    Restart,
    Unknown(i32),
}

impl AppStatusCode {
    fn from_status_code(status_code: i32) -> Self {
        match status_code {
            0 => Self::Exit,
            100 => Self::Restart,
            code => Self::Unknown(code),
        }
    }

    fn into_status_code(self) -> i32 {
        match self {
            AppStatusCode::Exit => 0,
            AppStatusCode::Restart => 100,
            AppStatusCode::Unknown(code) => code,
        }
    }
}

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const GLOBAL_CSS: &str = include_str!("../res/app.css");
const ABOUT_IMAGE: &[u8] = include_bytes!("../res/about.png");

type PosTable = ChiaTable;

#[derive(Debug)]
enum AppInput {
    BackendNotification(BackendNotification),
    Configuration(ConfigurationOutput),
    Running(RunningOutput),
    OpenLogFolder,
    OpenReconfiguration,
    ShowAboutDialog,
    InitialConfiguration,
    StartUpgrade,
    Restart,
}

#[derive(Debug)]
enum AppCommandOutput {
    BackendNotification(BackendNotification),
    Restart,
}

enum View {
    Welcome,
    Upgrade { chain_name: String },
    Loading,
    Configuration,
    Reconfiguration,
    Running,
    Stopped(Option<anyhow::Error>),
    Error(anyhow::Error),
}

impl View {
    fn title(&self) -> &'static str {
        match self {
            Self::Welcome => "Welcome",
            Self::Upgrade { .. } => "Upgrade",
            Self::Loading => "Loading",
            Self::Configuration => "Configuration",
            Self::Reconfiguration => "Reconfiguration",
            Self::Running => "Running",
            Self::Stopped(_) => "Stopped",
            Self::Error(_) => "Error",
        }
    }
}

#[derive(Debug, Default)]
enum StatusBarNotification {
    #[default]
    None,
    Warning {
        message: String,
        /// Whether to show restart button
        restart: bool,
    },
    Error(String),
}

impl StatusBarNotification {
    fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    fn css_classes() -> &'static [&'static str] {
        &["label", "warning-label", "error-label"]
    }

    fn css_class(&self) -> &'static str {
        match self {
            Self::None => "label",
            Self::Warning { .. } => "warning-label",
            Self::Error(_) => "error-label",
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::None => "",
            Self::Warning { message, .. } | Self::Error(message) => message.as_str(),
        }
    }

    fn restart_button(&self) -> bool {
        match self {
            Self::Warning { restart, .. } => *restart,
            _ => false,
        }
    }
}

struct AppInit {
    app_data_dir: Option<PathBuf>,
    exit_status_code: Arc<Mutex<AppStatusCode>>,
    minimize_on_start: bool,
}

// TODO: Efficient updates with tracker
struct App {
    current_view: View,
    current_raw_config: Option<RawConfig>,
    status_bar_notification: StatusBarNotification,
    backend_action_sender: mpsc::Sender<BackendAction>,
    new_version: Controller<NewVersion>,
    loading_view: Controller<LoadingView>,
    configuration_view: Controller<ConfigurationView>,
    running_view: Controller<RunningView>,
    menu_popover: gtk::Popover,
    about_dialog: gtk::AboutDialog,
    app_data_dir: Option<PathBuf>,
    exit_status_code: Arc<Mutex<AppStatusCode>>,
    // Stored here so `Drop` is called on this future as well, preventing exit until everything shuts down gracefully
    _background_tasks: Box<dyn Future<Output = ()>>,
}

#[relm4::component(async)]
impl AsyncComponent for App {
    type Init = AppInit;
    type Input = AppInput;
    type Output = ();
    type CommandOutput = AppCommandOutput;

    view! {
        gtk::Window {
            set_decorated: false,
            set_resizable: false,
            set_size_request: (800, 600),
            #[watch]
            set_title: Some(&format!("{} - Space Acres {}", model.current_view.title(), env!("CARGO_PKG_VERSION"))),

            gtk::Box {
                set_orientation: gtk::Orientation::Vertical,

                gtk::HeaderBar {
                    pack_end = &gtk::Box {
                        set_spacing: 10,

                        model.new_version.widget().clone(),

                        gtk::MenuButton {
                            set_direction: gtk::ArrowType::None,
                            set_icon_name: icon_name::MENU_LARGE,
                            #[wrap(Some)]
                            set_popover: menu_popover = &gtk::Popover {
                                set_halign: gtk::Align::End,
                                set_position: gtk::PositionType::Bottom,

                                gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,
                                    set_spacing: 5,

                                    gtk::Button {
                                        connect_clicked => AppInput::OpenLogFolder,
                                        set_label: "Show logs in file manager",
                                        set_visible: model.app_data_dir.is_some(),
                                    },

                                    gtk::Button {
                                        connect_clicked => AppInput::OpenReconfiguration,
                                        set_label: "Update configuration",
                                        #[watch]
                                        set_visible: model.current_raw_config.is_some(),
                                    },

                                    gtk::Button {
                                        connect_clicked => AppInput::ShowAboutDialog,
                                        set_label: "About",
                                    },
                                },
                            },
                        },
                    },
                },

                gtk::Box {
                    set_margin_all: 10,
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 10,

                    #[transition = "SlideLeftRight"]
                    match &model.current_view {
                        View::Welcome => gtk::Box {
                            set_margin_all: 10,
                            set_orientation: gtk::Orientation::Vertical,
                            set_spacing: 20,

                            gtk::Image {
                                set_height_request: 256,
                                set_from_pixbuf: Some(
                                    &gtk::gdk_pixbuf::Pixbuf::from_read(ABOUT_IMAGE)
                                        .expect("Statically correct image; qed")
                                ),
                            },

                            gtk::Label {
                                set_label: indoc::indoc! {"
                                    Space Acres is an opinionated GUI application for farming on Subspace Network.

                                    Before continuing you need 3 things:
                                    ✔ Wallet address where you'll receive rewards (use Subwallet, polkadot{.js} extension or any other wallet compatible with Substrate chain)
                                    ✔ 100G of space on a good quality SSD to store node data
                                    ✔ any SSDs (or multiple) with as much space as you can afford for farming purposes, this is what will generate rewards"
                                },
                                set_wrap: true,
                            },

                            gtk::Box {
                                set_halign: gtk::Align::End,


                                gtk::Button {
                                    add_css_class: "suggested-action",
                                    connect_clicked => AppInput::InitialConfiguration,

                                    gtk::Label {
                                        set_label: "Continue",
                                        set_margin_all: 10,
                                    },
                                },
                            },
                        },
                        View::Upgrade { chain_name } => gtk::Box {
                            set_margin_all: 10,
                            set_orientation: gtk::Orientation::Vertical,
                            set_spacing: 20,

                            gtk::Image {
                                set_height_request: 256,
                                set_from_pixbuf: Some(
                                    &gtk::gdk_pixbuf::Pixbuf::from_read(ABOUT_IMAGE)
                                        .expect("Statically correct image; qed")
                                ),
                            },

                            gtk::Label {
                                set_label: indoc::indoc! {"
                                    Thanks for choosing Space Acres again!

                                    The chain you were running before upgrade is no longer compatible with this release of Space Acres, likely because you were participating in the previous version of Subspace Network.

                                    But fear not, you can upgrade to currently supported network with a single click of a button!"
                                },
                                set_wrap: true,
                            },

                            gtk::Box {
                                set_halign: gtk::Align::End,


                                gtk::Button {
                                    add_css_class: "destructive-action",
                                    connect_clicked => AppInput::StartUpgrade,

                                    gtk::Label {
                                        #[watch]
                                        set_label: &format!("Upgrade to {chain_name}"),
                                        set_margin_all: 10,
                                    },
                                },
                            },
                        },
                        View::Loading => model.loading_view.widget().clone(),
                        View::Configuration | View::Reconfiguration => model.configuration_view.widget().clone(),
                        View::Running=> model.running_view.widget().clone(),
                        View::Stopped(Some(error)) => {
                            // TODO: Better error handling
                            gtk::Label {
                                #[watch]
                                set_label: &format!("Stopped with error: {error}"),
                            }
                        }
                        View::Stopped(None) => {
                            gtk::Label {
                                set_label: "Stopped 🛑",
                            }
                        }
                        View::Error(error) => {
                            // TODO: Better error handling
                            gtk::Label {
                                #[watch]
                                set_label: &format!("Error: {error}"),
                            }
                        },
                    },

                    gtk::Box {
                        set_halign: gtk::Align::Center,
                        set_spacing: 10,
                        #[watch]
                        set_visible: !model.status_bar_notification.is_none(),

                        #[name(status_bar_notification_label)]
                        gtk::Label {
                            #[track = "!status_bar_notification_label.has_css_class(model.status_bar_notification.css_class())"]
                            add_css_class: {
                                for css_class in StatusBarNotification::css_classes() {
                                    status_bar_notification_label.remove_css_class(css_class);
                                }

                                model.status_bar_notification.css_class()
                            },
                            #[watch]
                            set_label: model.status_bar_notification.message(),
                        },

                        gtk::Button {
                            add_css_class: "suggested-action",
                            connect_clicked => AppInput::Restart,
                            set_label: "Restart",
                            #[watch]
                            set_visible: model.status_bar_notification.restart_button(),
                        },
                    },
                },
            }
        }
    }

    async fn init(
        init: Self::Init,
        root: Self::Root,
        sender: AsyncComponentSender<Self>,
    ) -> AsyncComponentParts<Self> {
        let (backend_action_sender, backend_action_receiver) = mpsc::channel(1);
        let (backend_notification_sender, mut backend_notification_receiver) = mpsc::channel(100);

        // Create and run backend in dedicated thread
        let backend_fut = run_future_in_dedicated_thread(
            move || backend::create(backend_action_receiver, backend_notification_sender),
            "backend".to_string(),
        )
        .expect("Must be able to spawn a thread");

        // Forward backend notifications as application inputs
        let message_forwarder_fut = AsyncJoinOnDrop::new(
            tokio::spawn({
                let sender = sender.clone();

                async move {
                    while let Some(notification) = backend_notification_receiver.next().await {
                        // TODO: This panics on shutdown because component is already shut down, this should be handled
                        //  more gracefully
                        sender.input(AppInput::BackendNotification(notification));
                    }
                }
            }),
            true,
        );

        let new_version = NewVersion::builder().launch(()).detach();

        let loading_view = LoadingView::builder().launch(()).detach();

        let configuration_view = ConfigurationView::builder()
            .launch(root.clone())
            .forward(sender.input_sender(), AppInput::Configuration);

        let running_view = RunningView::builder()
            .launch(RunningInit {
                // Not paused on start
                plotting_paused: false,
            })
            .forward(sender.input_sender(), AppInput::Running);

        let about_dialog = gtk::AboutDialog::builder()
            .title("About")
            .program_name("Space Acres")
            .version(env!("CARGO_PKG_VERSION"))
            .authors(env!("CARGO_PKG_AUTHORS").split(':').collect::<Vec<_>>())
            // TODO: Use https://gitlab.gnome.org/GNOME/gtk/-/merge_requests/6643 once available
            .license("Zero-Clause BSD: https://opensource.org/license/0bsd/")
            .website(env!("CARGO_PKG_REPOSITORY"))
            .website_label("GitHub")
            .comments(env!("CARGO_PKG_DESCRIPTION"))
            .logo(&gtk::gdk::Texture::for_pixbuf(
                &gtk::gdk_pixbuf::Pixbuf::from_read(ABOUT_IMAGE)
                    .expect("Statically correct image; qed"),
            ))
            .system_information({
                let config_directory = dirs::config_local_dir()
                    .map(|config_local_dir| {
                        config_local_dir
                            .join(env!("CARGO_PKG_NAME"))
                            .display()
                            .to_string()
                    })
                    .unwrap_or_else(|| "Unknown".to_string());
                let data_directory = dirs::data_local_dir()
                    .map(|data_local_dir| {
                        data_local_dir
                            .join(env!("CARGO_PKG_NAME"))
                            .display()
                            .to_string()
                    })
                    .unwrap_or_else(|| "Unknown".to_string());

                format!(
                    "Config directory: {config_directory}\n\
                    Data directory (including logs): {data_directory}",
                )
            })
            .transient_for(&root)
            .build();
        about_dialog.connect_close_request(|about_dialog| {
            about_dialog.hide();
            gtk::glib::Propagation::Stop
        });

        let mut model = Self {
            current_view: View::Loading,
            current_raw_config: None,
            status_bar_notification: StatusBarNotification::None,
            backend_action_sender,
            new_version,
            loading_view,
            configuration_view,
            running_view,
            // Hack to initialize a field before this data structure is used
            menu_popover: gtk::Popover::default(),
            about_dialog,
            app_data_dir: init.app_data_dir,
            exit_status_code: init.exit_status_code,
            _background_tasks: Box::new(async move {
                // Order is important here, if backend is dropped first, there will be an annoying panic in logs due to
                // notification forwarder sending notification to the component that is already shut down
                select! {
                    _ = message_forwarder_fut.fuse() => {
                        warn!("Message forwarder exited");
                    }
                    _ = backend_fut.fuse() => {
                        warn!("Backend exited");
                    }
                }
            }),
        };

        let widgets = view_output!();

        model.menu_popover = widgets.menu_popover.clone();

        if init.minimize_on_start {
            root.minimize();
        }

        AsyncComponentParts { model, widgets }
    }

    async fn update(
        &mut self,
        input: Self::Input,
        sender: AsyncComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match input {
            AppInput::OpenLogFolder => {
                self.open_log_folder();
            }
            AppInput::BackendNotification(notification) => {
                self.process_backend_notification(notification);
            }
            AppInput::Configuration(configuration_output) => {
                self.process_configuration_output(configuration_output)
                    .await;
            }
            AppInput::Running(running_output) => {
                self.process_running_output(running_output).await;
            }
            AppInput::OpenReconfiguration => {
                self.menu_popover.hide();
                if let Some(raw_config) = self.current_raw_config.clone() {
                    self.configuration_view
                        .emit(ConfigurationInput::Reconfigure(raw_config));
                    self.current_view = View::Reconfiguration;
                }
            }
            AppInput::ShowAboutDialog => {
                self.menu_popover.hide();
                self.about_dialog.show();
            }
            AppInput::InitialConfiguration => {
                self.current_view = View::Configuration;
            }
            AppInput::StartUpgrade => {
                let raw_config = self
                    .current_raw_config
                    .clone()
                    .expect("Must have raw config when corresponding button is clicked; qed");
                sender.command(move |sender, shutdown_receiver| async move {
                    Self::do_upgrade(sender, shutdown_receiver, raw_config).await;
                });
                self.current_view = View::Loading;
            }
            AppInput::Restart => {
                *self.exit_status_code.lock() = AppStatusCode::Restart;
                relm4::main_application().quit();
            }
        }
    }

    async fn update_cmd(
        &mut self,
        input: Self::CommandOutput,
        _sender: AsyncComponentSender<Self>,
        _root: &Self::Root,
    ) {
        self.process_command(input);
    }
}

impl App {
    fn open_log_folder(&mut self) {
        let Some(app_data_dir) = &self.app_data_dir else {
            return;
        };
        if let Err(error) = open::that_detached(app_data_dir) {
            error!(%error, path = %app_data_dir.display(), "Failed to open logs folder");
        }
    }
    fn process_backend_notification(&mut self, notification: BackendNotification) {
        match notification {
            // TODO: Render progress
            BackendNotification::Loading { step, progress: _ } => {
                self.current_view = View::Loading;
                self.status_bar_notification = StatusBarNotification::None;
                self.loading_view.emit(LoadingInput::BackendLoading(step));
            }
            BackendNotification::IncompatibleChain {
                raw_config,
                compatible_chain,
            } => {
                self.current_raw_config.replace(raw_config);
                self.current_view = View::Upgrade {
                    chain_name: compatible_chain,
                };
            }
            BackendNotification::NotConfigured => {
                self.current_view = View::Welcome;
            }
            BackendNotification::ConfigurationIsInvalid { error, .. } => {
                self.status_bar_notification =
                    StatusBarNotification::Error(format!("Configuration is invalid: {error}"));
            }
            BackendNotification::ConfigSaveResult(result) => match result {
                Ok(()) => {
                    self.status_bar_notification = StatusBarNotification::Warning {
                        message:
                            "Application restart is needed for configuration changes to take effect"
                                .to_string(),
                        restart: true,
                    };
                }
                Err(error) => {
                    self.status_bar_notification = StatusBarNotification::Error(format!(
                        "Failed to save configuration changes: {error}"
                    ));
                }
            },
            BackendNotification::Running {
                config: _,
                raw_config,
                best_block_number,
                reward_address_balance,
                initial_farm_states,
                farm_during_initial_plotting,
                chain_info,
            } => {
                self.current_raw_config.replace(raw_config.clone());
                self.current_view = View::Running;
                self.running_view.emit(RunningInput::Initialize {
                    best_block_number,
                    reward_address_balance,
                    initial_farm_states,
                    farm_during_initial_plotting,
                    raw_config,
                    chain_info,
                });
            }
            BackendNotification::Node(node_notification) => {
                self.running_view
                    .emit(RunningInput::NodeNotification(node_notification));
            }
            BackendNotification::Farmer(farmer_notification) => {
                self.running_view
                    .emit(RunningInput::FarmerNotification(farmer_notification));
            }
            BackendNotification::Stopped { error } => {
                self.current_view = View::Stopped(error);
            }
            BackendNotification::IrrecoverableError { error } => {
                self.current_view = View::Error(error);
            }
        }
    }

    async fn process_configuration_output(&mut self, configuration_output: ConfigurationOutput) {
        match configuration_output {
            ConfigurationOutput::StartWithNewConfig(raw_config) => {
                if let Err(error) = self
                    .backend_action_sender
                    .send(BackendAction::NewConfig { raw_config })
                    .await
                {
                    self.current_view =
                        View::Error(anyhow::anyhow!("Failed to send config to backend: {error}"));
                }
            }
            ConfigurationOutput::ConfigUpdate(raw_config) => {
                self.current_raw_config.replace(raw_config.clone());
                // Config is updated when application is already running, switch to corresponding screen
                self.current_view = View::Running;
                if let Err(error) = self
                    .backend_action_sender
                    .send(BackendAction::NewConfig { raw_config })
                    .await
                {
                    self.current_view =
                        View::Error(anyhow::anyhow!("Failed to send config to backend: {error}"));
                }
            }
            ConfigurationOutput::Back => {
                // Back to welcome screen
                self.current_view = View::Welcome;
            }
            ConfigurationOutput::Close => {
                // Configuration view is closed when application is already running, switch to corresponding screen
                self.current_view = View::Running;
            }
        }
    }

    async fn process_running_output(&mut self, running_output: RunningOutput) {
        match running_output {
            RunningOutput::PausePlotting(pause_plotting) => {
                if let Err(error) = self
                    .backend_action_sender
                    .send(BackendAction::Farmer(FarmerAction::PausePlotting(
                        pause_plotting,
                    )))
                    .await
                {
                    self.current_view = View::Error(anyhow::anyhow!(
                        "Failed to send pause plotting to backend: {error}"
                    ));
                }
            }
        }
    }

    fn process_command(&mut self, input: AppCommandOutput) {
        match input {
            AppCommandOutput::BackendNotification(notification) => {
                self.process_backend_notification(notification);
            }
            AppCommandOutput::Restart => {
                *self.exit_status_code.lock() = AppStatusCode::Restart;
                relm4::main_application().quit();
            }
        }
    }

    async fn do_upgrade(
        sender: Sender<AppCommandOutput>,
        shutdown_receiver: ShutdownReceiver,
        raw_config: RawConfig,
    ) {
        shutdown_receiver
            .register(async move {
                let (mut backend_notification_sender, mut backend_notification_receiver) =
                    mpsc::channel(100);

                tokio::spawn({
                    let sender = sender.clone();

                    async move {
                        while let Some(notification) = backend_notification_receiver.next().await {
                            if sender
                                .send(AppCommandOutput::BackendNotification(notification))
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                });

                if let Err(error) = wipe(&raw_config, &mut backend_notification_sender).await {
                    error!(%error, "Wiping error");
                }

                let _ = sender.send(AppCommandOutput::Restart);
            })
            .drop_on_shutdown()
            .await
    }
}

#[derive(Debug, Parser)]
#[clap(about, version)]
struct Cli {
    /// Used for startup to minimize the window
    #[arg(long)]
    startup: bool,
    /// Used by child process such that supervisor parent process can control it
    #[arg(long)]
    child_process: bool,
    /// Show uninstall dialog and delete configuration and logs
    #[arg(long)]
    uninstall: bool,
    /// The rest of the arguments that will be sent to GTK4 as is
    #[arg(raw = true)]
    gtk_arguments: Vec<String>,
}

impl Cli {
    fn run(self) -> ExitCode {
        if self.uninstall {
            if cfg!(windows)
                && MessageDialog::new()
                    .set_type(MessageType::Info)
                    .set_title("Uninstall")
                    .set_text("Delete Space Acres configuration and logs for all users?")
                    .show_confirm()
                    .unwrap_or_default()
            {
                if let Some(system_drive) = std::env::var_os("SystemDrive") {
                    // Workaround for https://github.com/rust-lang/rust-clippy/issues/12244
                    #[allow(clippy::all)]
                    let users_dir = std::path::PathBuf::from(system_drive).join("\\Users");
                    if let Ok(entries) = fs::read_dir(users_dir) {
                        for entry in entries.flatten() {
                            let _ = fs::remove_dir_all(
                                entry.path().join("AppData").join("Local).join(env!("CARGO_PKG_NAME")),
                            );
                        }
                    }
                }
            }
            ExitCode::SUCCESS
        } else if self.child_process {
            ExitCode::from(self.app().into_status_code() as u8)
        } else {
            self.supervisor().report()
        }
    }

    fn app(self) -> AppStatusCode {
        let maybe_app_data_dir = Self::app_data_dir();

        {
            let layer = tracing_subscriber::fmt::layer()
                // TODO: Workaround for https://github.com/tokio-rs/tracing/issues/2214, also on
                //  Windows terminal doesn't support the same colors as bash does
                .with_ansi(if cfg!(windows) {
                    false
                } else {
                    supports_color::on(supports_color::Stream::Stderr).is_some()
                });
            let filter = EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy();
            if WINDOWS_SUBSYSTEM_WINDOWS {
                if let Some(app_data_dir) = &maybe_app_data_dir {
                    let logger = std::sync::Mutex::new(Self::new_logger(app_data_dir));
                    let layer = layer.with_writer(logger);

                    tracing_subscriber::registry()
                        .with(layer.with_filter(filter))
                        .init();
                } else {
                    tracing_subscriber::registry()
                        .with(layer.with_filter(filter))
                        .init();
                }
            } else {
                tracing_subscriber::registry()
                    .with(layer.with_filter(filter))
                    .init();
            }
        }

        info!(
            "Starting {} {}",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION")
        );

        // The default in `relm4` is `1`, set this back to Tokio's default
        RELM_THREADS
            .set(
                available_parallelism()
                    .map(|cores| cores.get())
                    .unwrap_or(1),
            )
            .expect("The first thing in the app, is not set; qed");

        let app = RelmApp::new("network.subspace.space_acres");
        let app = app.with_args({
            let mut args = self.gtk_arguments;
            // Application itself is expected as the first argument
            args.insert(0, env::args().next().expect("Guaranteed to exist; qed"));
            args
        });

        app.set_global_css(GLOBAL_CSS);
        relm4_icons::initialize_icons();

        // Prefer dark theme in cross-platform way if environment is configured that way
        if let Some(settings) = gtk::Settings::default() {
            if matches!(dark_light::detect(), dark_light::Mode::Dark) {
                settings.set_gtk_application_prefer_dark_theme(true);
            }
        }

        let exit_status_code = Arc::new(Mutex::new(AppStatusCode::Exit));

        app.run_async::<App>(AppInit {
            app_data_dir: maybe_app_data_dir,
            exit_status_code: Arc::clone(&exit_status_code),
            minimize_on_start: self.startup,
        });

        let exit_status_code = *exit_status_code.lock();
        info!(
            ?exit_status_code,
            "Exiting {} {}",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION")
        );
        exit_status_code
    }

    fn supervisor(mut self) -> io::Result<()> {
        let maybe_app_data_dir = Self::app_data_dir();

        let program = Self::child_program()?;

        loop {
            let mut args = vec!["--child-process".to_string()];
            if self.startup {
                // In case of restart we no longer want to minimize the app
                self.startup = false;

                args.push("--startup".to_string());
            }
            args.push("--".to_string());
            args.extend_from_slice(&self.gtk_arguments);

            let exit_status = if let Some(app_data_dir) = (!WINDOWS_SUBSYSTEM_WINDOWS)
                .then_some(maybe_app_data_dir.as_ref())
                .flatten()
            {
                let mut expression = cmd(&program, args)
                    .stderr_to_stdout()
                    // We use non-zero status codes, and they don't mean error necessarily
                    .unchecked()
                    .reader()?;

                let mut logger = Self::new_logger(app_data_dir);

                let mut log_read_buffer = vec![0u8; LOG_READ_BUFFER];

                let mut stdout = io::stdout();
                loop {
                    match expression.read(&mut log_read_buffer) {
                        Ok(bytes_count) => {
                            if bytes_count == 0 {
                                break;
                            }

                            let write_result: io::Result<()> = try {
                                stdout.write_all(&log_read_buffer[..bytes_count])?;
                                logger.write_all(&log_read_buffer[..bytes_count])?;
                            };

                            if let Err(error) = write_result {
                                eprintln!("Error while writing output of child process: {error}");
                                break;
                            }
                        }
                        Err(error) => {
                            if error.kind() == io::ErrorKind::Interrupted {
                                // Try again
                                continue;
                            }
                            eprintln!("Error while reading output of child process: {error}");
                            break;
                        }
                    }
                }

                stdout.flush()?;
                if let Err(error) = logger.flush() {
                    eprintln!("Error while flushing logs: {error}");
                }

                match expression.try_wait()? {
                    Some(output) => output.status,
                    None => {
                        return Err(io::Error::new(
                            io::ErrorKind::Other,
                            "Logs writing ended before child process did, exiting",
                        ));
                    }
                }
            } else if WINDOWS_SUBSYSTEM_WINDOWS {
                cmd(&program, args)
                    .stdin_null()
                    .stdout_null()
                    .stderr_null()
                    // We use non-zero status codes and they don't mean error necessarily
                    .unchecked()
                    .run()?
                    .status
            } else {
                eprintln!("App data directory doesn't exist, not creating log file");
                cmd(&program, args)
                    // We use non-zero status codes and they don't mean error necessarily
                    .unchecked()
                    .run()?
                    .status
            };

            match exit_status.code() {
                Some(status_code) => match AppStatusCode::from_status_code(status_code) {
                    AppStatusCode::Exit => {
                        eprintln!("Application exited gracefully");
                        break;
                    }
                    AppStatusCode::Restart => {
                        eprintln!("Restarting application");
                        continue;
                    }
                    AppStatusCode::Unknown(status_code) => {
                        eprintln!("Application exited with unexpected status code {status_code}");
                        process::exit(status_code);
                    }
                },
                None => {
                    eprintln!("Application terminated by signal");
                    break;
                }
            }
        }

        Ok(())
    }

    fn app_data_dir() -> Option<PathBuf> {
        dirs::data_local_dir()
            .map(|data_local_dir| data_local_dir.join(env!("CARGO_PKG_NAME")))
            .and_then(|app_data_dir| {
                if !app_data_dir.exists() {
                    if let Err(error) = fs::create_dir_all(&app_data_dir) {
                        eprintln!(
                            "App data directory \"{}\" doesn't exist and can't be created: {}",
                            app_data_dir.display(),
                            error
                        );
                        return None;
                    }
                }

                Some(app_data_dir)
            })
    }

    fn new_logger(app_data_dir: &Path) -> FileRotate<AppendCount> {
        FileRotate::new(
            app_data_dir.join("space-acres.log"),
            AppendCount::new(LOG_FILE_LIMIT_COUNT),
            ContentLimit::Bytes(LOG_FILE_LIMIT_SIZE),
            Compression::OnRotate(0),
            #[cfg(unix)]
            Some(0o600),
        )
    }

    #[cfg(target_arch = "x86_64")]
    fn child_program() -> io::Result<PathBuf> {
        let program = env::current_exe()?;

        if !std::arch::is_x86_feature_detected!("xsavec") {
            return Ok(program);
        }

        let mut maybe_extension = program.extension();
        let Some(file_name) = program.file_stem() else {
            return Ok(program);
        };

        let mut file_name = file_name.to_os_string();

        if let Some(extension) = maybe_extension
            && extension != "exe"
        {
            file_name = program
                .file_name()
                .expect("Checked above; qed")
                .to_os_string();
            maybe_extension = None;
        }

        file_name.push("-modern");
        if let Some(extension) = maybe_extension {
            file_name.push(".");
            file_name.push(extension);
        }
        let mut modern_program = program.clone();
        modern_program.set_file_name(file_name);
        if modern_program.exists() {
            Ok(modern_program)
        } else {
            Ok(program)
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn child_program() -> io::Result<PathBuf> {
        env::current_exe()
    }
}

fn main() -> ExitCode {
    // TODO: This is a hack to work around https://github.com/quinn-rs/quinn/issues/1750, should be
    //  removed once fixed upstream
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", "info,quinn_udp=error");
    }
    Cli::parse().run()
}
