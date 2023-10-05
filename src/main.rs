use dashmap::DashMap;
use eyre::{eyre, Context, Result};
use reqwest::Client;
use std::{net::IpAddr, path::PathBuf, str::FromStr, sync::Arc};
use tokio::sync::RwLock;

use ical_merger::{
    calendars::Calendar,
    config::{read_config_file, ApplicationConfig},
};
use rocket::{get, http::ContentType, log::LogLevel, response::Responder, routes, Config, State};

#[derive(clap::Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[command(flatten)]
    verbose: clap_verbosity_flag::Verbosity,

    #[command(subcommand)]
    sub: Command,
}

fn quad0() -> IpAddr {
    IpAddr::from_str("0.0.0.0").unwrap()
}

#[derive(clap::Subcommand, Debug, Clone, PartialEq, Eq)]
enum Command {
    Check {
        #[arg(short, long)]
        path: Option<PathBuf>,
    },

    Serve {
        #[arg(short, long, default_value_t = 8080, env = "PORT")]
        port: u16,

        #[arg(short, long, default_value_t = quad0(), env = "ADDRESS")]
        listen: IpAddr,
    },
}

fn main() -> Result<()> {
    let args = <Args as clap::Parser>::parse();
    pretty_env_logger::formatted_timed_builder()
        .filter_level(args.verbose.log_level_filter())
        .init();

    let config = read_config_file()?;
    let (listen, port) = match args.sub {
        Command::Check { .. } => return Ok(()),
        Command::Serve {
            port,
            listen: listen,
        } => (listen, port),
    };
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(rocket(config, listen, port))?;
    Ok(())
}

async fn rocket(config: ApplicationConfig, listen: IpAddr, port: u16) -> Result<()> {
    let config = Arc::new(RwLock::new(config));

    let cache: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    tokio::spawn(worker_thread(config.clone(), cache.clone(), Client::new()));

    let mut cfg = Config::default();
    cfg.profile = Config::RELEASE_PROFILE;
    cfg.port = port;
    cfg.log_level = LogLevel::Debug;
    cfg.workers = 2;
    cfg.address = listen;

    rocket::custom(cfg)
        .mount("/", routes![calendar])
        .manage(cache)
        .manage(config)
        .launch()
        .await?;
    Ok(())
}

async fn worker_thread(
    config: Arc<RwLock<ApplicationConfig>>,
    cache: Arc<DashMap<String, String>>,
    client: Client,
) -> Result<()> {
    let interval_time = {
        let config = config.read().await.clone();
        if config.fetch_on_demand {
            return Err(eyre!("Done"));
        }
        config.fetch_interval_seconds
    };
    let mut interval =
        tokio::time::interval(tokio::time::Duration::from_secs(interval_time.ok_or(
            eyre!("Worker thread not running, since calendars are not polled"),
        )?));
    loop {
        interval.tick().await;
        // for all calendars in the config, fetch them
        let config = config.read().await.clone();
        // if fetch_on_demand is true, continue
        if config.fetch_on_demand {
            continue;
        }
        for (path, calendar_config) in config.calendars {
            let _ = Calendar::from_config(client.clone(), calendar_config)
                .await
                .map(|calendar| cache.insert(path.clone(), calendar.to_string()))
                .wrap_err(eyre!("Failed to build calendar {}", path))
                .map_err(|e| println!("{:?}", e));
        }
    }
}

#[get("/<ident..>")]
async fn calendar<'a>(
    cache: &'a State<Arc<DashMap<String, String>>>,
    config: &'a State<Arc<RwLock<ApplicationConfig>>>,
    ident: PathBuf,
) -> impl Responder<'a, 'a> {
    let ident = ident.to_string_lossy().to_string();
    let config = { config.read().await.clone() };
    if config.fetch_on_demand {
        // populate cache with this calendar, iff it exists
        if !config.calendars.contains_key(&ident) {
            return Err("Calendar not found".to_string());
        }
        let _ = Calendar::from_config(Client::new(), config.calendars[&ident].clone())
            .await
            .map(|calendar| cache.insert(ident.clone(), calendar.to_string()))
            .wrap_err(eyre!("Failed to build calendar {}", ident))
            .map_err(|e| println!("{:?}", e));
    }
    let response = cache
        .get(&ident)
        .ok_or_else(|| "No calendar found".to_string())?
        .to_string();
    Ok::<(ContentType, String), String>((ContentType::Calendar, response))
}
