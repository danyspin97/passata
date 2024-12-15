mod pipewire;

use std::{
    path::PathBuf,
    process::{exit, Command},
    time::{Duration, Instant},
};

use color_eyre::{
    eyre::{ensure, eyre, ContextCompat, WrapErr},
    Result,
};
use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use flexi_logger::{Duplicate, FileSpec, Logger};
use lexopt::Arg::{Long, Short};
use log::debug;
use nix::unistd::fork;
use notify_rust::Notification;
use serde::{Deserialize, Serialize};
use smithay_client_toolkit::reexports::{
    calloop::Dispatcher,
    client::{Connection, Dispatch},
    protocols::ext::idle_notify::v1::client::{
        ext_idle_notification_v1::{self, ExtIdleNotificationV1},
        ext_idle_notifier_v1::ExtIdleNotifierV1,
    },
};
use smithay_client_toolkit::{
    delegate_registry, delegate_seat,
    reexports::{
        calloop::{
            timer::{TimeoutAction, Timer},
            EventLoop,
        },
        calloop_wayland_source::WaylandSource,
        client::globals::registry_queue_init,
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{SeatHandler, SeatState},
};
use xdg::BaseDirectories;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Config {
    #[serde(rename = "interval", with = "humantime_serde")]
    pub work_interval: Duration,
    #[serde(with = "humantime_serde")]
    pub short_break: Duration,
    #[serde(with = "humantime_serde")]
    pub long_break: Option<Duration>,
    pub short_breaks_before_long_break: Option<u8>,
    #[serde(with = "humantime_serde")]
    pub idle_timeout: Option<Duration>,
}

#[derive(PartialEq)]
enum NextEvent {
    Work,
    ShortBreak,
    LongBreak,
}

struct Args {
    config: Option<PathBuf>,
    daemon: bool,
}

fn parse_args() -> Result<Args, lexopt::Error> {
    let mut config: Option<PathBuf> = None;
    let mut daemon = false;
    let mut parser = lexopt::Parser::from_env();
    while let Some(arg) = parser.next()? {
        match arg {
            Short('c') | Long("config") => {
                config = Some(PathBuf::from(parser.value()?));
            }
            Short('d') | Long("daemon") => {
                daemon = true;
            }
            _ => return Err(arg.unexpected()),
        }
    }

    Ok(Args { config, daemon })
}

enum IdleStatus {
    Idled,
    Resumed,
}

struct Passata {
    next_event: NextEvent,
    current_short_breaks: u8,
    config: Config,
    registry_state: RegistryState,
    seat_state: SeatState,
    /// When the system is currently in idle, the remaining time before a break is stored here
    time_passed: Option<Duration>,
    /// Determine when the timer was started
    timer_started: Instant,
    /// either Idled or Resumed
    idle_status: Option<IdleStatus>,
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let xdg = BaseDirectories::with_prefix("passata")?;

    let mut logger = Logger::try_with_env_or_str("info")?;

    if args.daemon {
        logger = logger.log_to_file(FileSpec::default().directory(xdg.get_state_home()));
        match unsafe { fork()? } {
            nix::unistd::ForkResult::Parent { child: _ } => exit(0),
            nix::unistd::ForkResult::Child => {}
        }
    } else {
        logger = logger.duplicate_to_stderr(Duplicate::Warn);
    }

    logger.start()?;

    let config_file = args.config.unwrap_or(xdg.get_config_file("passata.toml"));
    ensure!(
        config_file.exists(),
        "Could not find config file {config_file:?}"
    );

    let config: Config = Figment::new()
        .merge(Toml::file(config_file))
        .merge(Env::prefixed("PASSATA"))
        .extract()?;

    let conn = Connection::connect_to_env().unwrap();

    let (globals, event_queue) = registry_queue_init(&conn).unwrap();
    let qh = event_queue.handle();

    let mut event_loop = EventLoop::<Passata>::try_new()?;
    WaylandSource::new(conn.clone(), event_queue)
        .insert(event_loop.handle())
        .map_err(|e| eyre!("insterting the wayland source into the event loop: {e}"))?;

    let registry_state = RegistryState::new(&globals);
    let mut state = Passata {
        next_event: NextEvent::ShortBreak,
        current_short_breaks: 0,
        config,
        registry_state,
        seat_state: SeatState::new(&globals, &qh),
        time_passed: None,
        timer_started: Instant::now(),
        idle_status: None,
    };

    let idle_notifier = globals.bind::<ExtIdleNotifierV1, Passata, ()>(&qh, 1..=1, ())?;
    let seat = state.seat_state.seats().next().context("no seats found")?;
    if let Some(idle_timeout) = state.config.idle_timeout {
        idle_notifier.get_idle_notification(
            idle_timeout.as_millis().try_into().unwrap(),
            &seat,
            &qh,
            (),
        );
        //notification_cb);
    }

    let handle = event_loop.handle();
    let timer = Timer::from_duration(state.config.work_interval);
    let dispatcher = Dispatcher::new(timer, move |_instant, _, state: &mut Passata| {
        state.timer_started = Instant::now();
        match state.next_event {
            NextEvent::Work => {
                debug!("work again!");
                state.next_event = if let Some(short_breaks_before_long_break) =
                    state.config.short_breaks_before_long_break
                {
                    if state.current_short_breaks == short_breaks_before_long_break {
                        state.current_short_breaks = 0;
                        NextEvent::LongBreak
                    } else {
                        state.current_short_breaks += 1;
                        NextEvent::ShortBreak
                    }
                } else {
                    NextEvent::ShortBreak
                };
                TimeoutAction::ToDuration(state.config.work_interval)
            }
            NextEvent::ShortBreak => {
                debug!("short break!");
                state.next_event = NextEvent::Work;
                let summary_part = if let Some(short_breaks_before_long_break) =
                    state.config.short_breaks_before_long_break
                {
                    format!(
                        " ({}/{})",
                        state.current_short_breaks,
                        short_breaks_before_long_break + 1
                    )
                } else {
                    "".to_owned()
                };
                Notification::new()
                    .summary(&format!("Short break{}", summary_part))
                    .body("Take a pause!")
                    .show()
                    .unwrap();
                TimeoutAction::ToDuration(state.config.short_break)
            }
            NextEvent::LongBreak => {
                state.next_event = NextEvent::Work;
                Notification::new()
                    .summary("Long break")
                    .body("Take a long pause!")
                    .show()
                    .unwrap();
                TimeoutAction::ToDuration(state.config.long_break.unwrap())
            }
        }
    });
    let registration_token = handle.register_dispatcher(dispatcher.clone()).unwrap();

    loop {
        event_loop
            .dispatch(None, &mut state)
            .context("dispatching the event loop")?;

        // don't process the idle events when a break is currently going on
        if NextEvent::Work != state.next_event {
            if let Some(idle_event) = state.idle_status.take() {
                match idle_event {
                    IdleStatus::Idled => {
                        handle.disable(&registration_token)?;
                        state.time_passed = Some(state.timer_started.elapsed());
                    }
                    IdleStatus::Resumed => {
                        let time_left = state.config.work_interval - state.time_passed.unwrap();
                        dispatcher.as_source_mut().set_duration(time_left);
                        debug!("time left before break: {time_left:?}");
                        handle.enable(&registration_token)?;
                        state.timer_started = Instant::now();
                        let time_left = time_left.as_secs();
                        let time_left = if time_left < 60 {
                            time_left
                        } else {
                            time_left - time_left % 60
                        };
                        Notification::new()
                            .summary(&format!(
                                "{} until next break",
                                humantime::format_duration(Duration::from_secs(time_left))
                            ))
                            .body("Take a pause!")
                            .show()
                            .unwrap();
                    }
                }
            }
        }
    }
}

impl SeatHandler for Passata {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _seat: smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat,
    ) {
    }

    fn new_capability(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _seat: smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat,
        _capability: smithay_client_toolkit::seat::Capability,
    ) {
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _seat: smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat,
        _capability: smithay_client_toolkit::seat::Capability,
    ) {
    }

    fn remove_seat(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _seat: smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat,
    ) {
    }
}

delegate_registry!(Passata);

delegate_seat!(Passata);

impl ProvidesRegistryState for Passata {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers!(SeatState);
}

impl Dispatch<ExtIdleNotifierV1, ()> for Passata {
    fn event(
        _state: &mut Self,
        _proxy: &ExtIdleNotifierV1,
        _event: <ExtIdleNotifierV1 as smithay_client_toolkit::reexports::client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for Passata {
    fn event(
        passata: &mut Self,
        _proxy: &ExtIdleNotificationV1,
        event: <ExtIdleNotificationV1 as smithay_client_toolkit::reexports::client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => {
                passata.idle_status = Some(IdleStatus::Idled);
                debug!("idled!");
            }
            ext_idle_notification_v1::Event::Resumed => {
                passata.idle_status = Some(IdleStatus::Resumed);
                debug!("resumed!");
            }
            _ => unreachable!(),
        }
    }
}
