use anyhow::format_err;
use chrono::{offset::Local, DateTime, NaiveDate};
use clap::{Parser, Subcommand, ValueEnum};
use futures::StreamExt;
use std::{
    io,
    path::{Path, PathBuf},
};
use timeflippers::{
    timeflip::{Entry, Event, TimeFlip},
    view, BluetoothSession, Config, Facet,
};
use tokio::{fs, select, signal};

async fn read_config(path: impl AsRef<Path>) -> anyhow::Result<Config> {
    let toml = fs::read_to_string(path).await?;
    let config: Config = toml::from_str(&toml)?;
    Ok(config)
}

fn facet_name(facet: &Facet, config: Option<&Config>) -> String {
    config
        .and_then(|config| config.sides[facet.index_zero()].name.clone())
        .unwrap_or(facet.to_string())
}

/// Communicate with a TimeFlip2 cube.
///
/// Note: Use `bluetoothctl` to pair (and potentially connect) the TimeFlip2.
/// Currently, the TimeFlip2's password is expected to be the default value.
#[derive(Parser)]
#[clap(about)]
struct Options {
    #[arg(short, long, help = "path to the timeflip.toml file")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum HistoryStyle {
    Lines,
    Tabular,
    Summarized,
}

#[derive(Subcommand)]
enum Command {
    /// Print the current battery level.
    Battery,
    /// Print logged TimeFlip events.
    History {
        #[arg(long, help = "read events from and write new events to file")]
        update: Option<PathBuf>,
        #[arg(
            long,
            help = "start reading with entry ID, latest event in `--update` takes precedence"
        )]
        start_with: Option<u32>,
        #[arg(long, help = "start displaying with entries after DATE (YYYY-MM-DD)")]
        since: Option<NaiveDate>,
        #[arg(long, help = "choose output style", default_value = "tabular")]
        style: HistoryStyle,
    },
    /// Print the facet currently facing up.
    Facet,
    /// Put the TimeFlip2 in lock mode.
    Lock,
    /// Release the TimeFlip2 from lock mode.
    Unlock,
    /// Subscribe to properties and get notified if they change.
    Notify {
        #[arg(long, help = "listen for battery events")]
        battery: bool,
        #[arg(long, help = "listen for facet events")]
        facet: bool,
        #[arg(long, help = "listen for double-tap events")]
        double_tap: bool,
        #[arg(long, help = "listen for log events")]
        log_event: bool,
    },
    /// Put the TimeFlip2 into pause mode.
    Pause,
    /// Release the TimeFlip2 from pause mode.
    Unpause,
    /// Print the TimeFlip2's system status.
    Status,
    /// Get the TimeFlip2's synchronization state.
    SyncState,
    /// Synchronize TimeFlip2. Do nothing if the cube reports it is synchronized.
    Sync,
    /// Get the TimeFlip2's current time.
    Time {
        #[arg(long, help = "set TimeFlip2's time to the current time")]
        set: bool,
    },
    /// Write config from the toml file to the TimeFlip2's memory.
    WriteConfig,
}

impl Command {
    async fn run(&self, timeflip: &mut TimeFlip, config: Option<Config>) -> anyhow::Result<()> {
        use Command::*;
        match self {
            Battery => {
                println!("Battery level: {}", timeflip.battery_level().await?);
            }
            History {
                update: update_file,
                start_with,
                style,
                since,
            } => {
                let config = config.ok_or(format_err!("config is mandatory for this command"))?;

                let (start_with, mut entries) = if let Some(file) = update_file {
                    match fs::read_to_string(file).await {
                        Ok(s) => {
                            let mut entries: Vec<Entry> = serde_json::from_str(&s)?;
                            entries.sort_by(|a, b| a.id.cmp(&b.id));
                            (
                                start_with
                                    .or_else(|| entries.last().map(|e| e.id))
                                    .unwrap_or(0),
                                entries,
                            )
                        }
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {
                            (start_with.unwrap_or(0), vec![])
                        }
                        Err(e) => return Err(e.into()),
                    }
                } else {
                    (start_with.unwrap_or(0), vec![])
                };

                let mut update = timeflip.read_history_since(start_with).await?;

                let new_ids = update.iter().map(|e| e.id).collect::<Vec<_>>();
                entries.retain(|entry| !new_ids.contains(&entry.id));
                entries.append(&mut update);

                if let Some(file) = update_file {
                    match serde_json::to_vec(&entries) {
                        Ok(json) => {
                            if let Err(e) = fs::write(file, json).await {
                                eprintln!("cannot update entries file {}: {e}", file.display());
                            }
                        }
                        Err(e) => eprintln!("cannot update entries file {}: {e}", file.display()),
                    }
                }

                let history = view::History::new(entries, config);
                let filtered = if let Some(since) = since {
                    let date = DateTime::<Local>::from_local(
                        since.and_hms_opt(0, 0, 0).expect("is a valid time"),
                        *Local::now().offset(),
                    );

                    history.since(date.into())
                } else {
                    history.all()
                };
                use HistoryStyle::*;
                match style {
                    Lines => println!("{}", filtered),
                    Tabular => println!("{}", filtered.table_by_day()),
                    Summarized => println!("{}", filtered.summarized()),
                }
            }
            Facet => {
                let facet = timeflip.facet().await?;
                println!("Currently up: {}", facet_name(&facet, config.as_ref()));
            }
            Lock => timeflip.lock().await?,
            Unlock => timeflip.unlock().await?,
            Notify {
                battery,
                facet,
                double_tap,
                log_event,
            } => {
                if *battery {
                    timeflip.subscribe_battery_level().await?;
                }
                if *facet {
                    timeflip.subscribe_facet().await?;
                }
                if *double_tap {
                    timeflip.subscribe_double_tap().await?;
                }
                if *log_event {
                    timeflip.subscribe_events().await?;
                }

                let mut stream = timeflip.event_stream().await?;
                loop {
                    match stream.next().await {
                        Some(Event::BatteryLevel(percent)) => println!("Battery Level {percent}"),
                        Some(Event::Event(event)) => println!("{event}"),
                        Some(Event::Facet(facet)) => {
                            println!("Currently Up: {}", facet_name(&facet, config.as_ref()))
                        }
                        Some(Event::DoubleTap { facet, pause }) => println!(
                            "Facet {} has {}",
                            facet_name(&facet, config.as_ref()),
                            if pause { "paused" } else { "started" }
                        ),
                        Some(Event::Disconnected) => {
                            println!("TimeFlip has disconnected");
                            break;
                        }
                        None => break,
                    }
                }
            }
            Pause => timeflip.pause().await?,
            Unpause => timeflip.unpause().await?,
            Status => {
                println!("System status: {:?}", timeflip.system_status().await?);
            }
            SyncState => {
                println!("Sync state: {:?}", timeflip.sync_state().await?);
            }
            Sync => {
                let config = config.ok_or(format_err!("config is mandatory for this command"))?;
                timeflip.sync(&config).await?;
            }
            Time { set } => {
                if *set {
                    let now = Local::now();
                    println!("Setting time to: {now}");
                    timeflip.set_time(now.into()).await?;
                } else {
                    let tz = Local::now().timezone();
                    let time = timeflip.time().await?;
                    println!("Time set on TimeFlip: {}", time.with_timezone(&tz));
                }
            }
            WriteConfig => {
                let config = config.ok_or(format_err!("config is mandatory for this command"))?;
                timeflip.write_config(config).await?;
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let opt = Options::parse();
    let config = if let Some(path) = opt.config {
        Some(read_config(path).await?)
    } else {
        None
    };

    let (mut bg_task, session) = BluetoothSession::new().await?;

    let mut timeflip =
        TimeFlip::connect(&session, config.as_ref().map(|c| c.password.clone())).await?;
    log::info!("connected");

    select! {
        _ = signal::ctrl_c() => {
            log::info!("shutting down");
        }
        res = &mut bg_task => {
            if let Err(e) =res {
                log::error!("bluetooth session background task exited with error: {e}");
            }
        }
        res = opt.cmd.run(&mut timeflip, config) => {
            res?;
        }
    }

    Ok(())
}
