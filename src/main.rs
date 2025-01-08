#[macro_use]
extern crate diesel;
#[macro_use]
extern crate diesel_migrations;

use std::path::PathBuf;

use eyre::{Report, Result, WrapErr};
use futures::stream::{self, StreamExt, TryStreamExt};
use google_calendar3::{api::Event, CalendarHub, client::Error as ApiError};
use log::LevelFilter;
use structopt::StructOpt;
use yup_oauth2::{
    authenticator::DefaultAuthenticator, noninteractive::NoninteractiveTokens,
    NoninteractiveAuthenticator,
};

mod event_iter;
mod page_iterator;

#[derive(Debug, StructOpt)]
struct Auth {
    #[structopt(short = "t", long = "token")]
    token: String,
}

impl Auth {
    async fn authenticator(&self) -> Result<DefaultAuthenticator> {
        let contents =
            std::fs::read(&self.token).wrap_err(format!("Couldn't load file: {}", self.token))?;
        let token = serde_json::from_slice::<NoninteractiveTokens>(&contents)
            .wrap_err(format!("Failed to parse token file: {}", self.token))?;
        Ok(NoninteractiveAuthenticator::builder(token).build().await?)
    }
}

#[derive(enum_utils::FromStr, Debug, Clone, Copy)]
#[enumeration(case_insensitive)]
enum Style {
    Debug,
    Json,
}

#[derive(thiserror::Error, Debug)]
#[error("Failed to parse {0} as style")]
struct StyleParseError(String);

impl Style {
    fn serialize(self, item: &Event) -> String {
        use Style::*;
        match self {
            Debug => format!("{:?}", item),
            Json => serde_json::to_string(item).unwrap(),
        }
    }

    fn parse(s: &str) -> Result<Self, StyleParseError> {
        use std::str::FromStr;

        Self::from_str(s).map_err(|()| StyleParseError(s.into()))
    }
}

impl std::fmt::Display for Style {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug, StructOpt)]
struct List {
    #[structopt(
        short = "c",
        long = "calendar",
        help = "Calendar ID",
        default_value = "primary"
    )]
    calendar: String,
    #[structopt(
        short = "n",
        long = "num",
        help = "Number of items to list",
        default_value = "50"
    )]
    num: usize,
    #[structopt(
        short = "s",
        long = "style",
        help = "Print style for item (options: debug, json)",
        default_value = "debug",
        parse(try_from_str = Style::parse),
    )]
    style: Style,
    #[structopt(
        short = "I",
        long = "no-index",
        help = "Don't print the index of each entry"
    )]
    no_index: bool,
}

impl List {
    async fn run(&self, hub: CalendarHub) -> Result<()> {
        let hub = &hub;
        let calendar = self.calendar.as_str();
        let iter = page_iterator::stream_pages(move |next_page_token| async move {
            let req = hub.events().list(calendar);
            let req = match next_page_token {
                Some(token) => req.page_token(token.as_str()),
                None => req,
            };
            let (_body, response) = req.doit().await?;
            
            Ok::<_, ApiError>((response.items.unwrap_or(vec![]), response.next_page_token))
        });
        iter.map_ok(|items: Vec<Event>| { stream::iter(items).map(|x| Ok::<_, ApiError>(x)) })
            .try_flatten()
            .take(self.num)
            .enumerate()
            .map(|(i, val)| val.map(|item| (i, item)))
            .try_for_each(|(i, item)| async move {
                let rep = self.style.serialize(&(item.into()));
                let idx = if self.no_index {
                    "".to_string()
                } else {
                    format!("{}: ", i)
                };
                println!("{}{}", idx, rep);
                Ok(())
            })
            .await
            .wrap_err("Failed to list items")?;
        Ok(())
    }
}

#[derive(Debug, StructOpt)]
enum Cmd {
    List(List),
}

#[derive(Debug, StructOpt)]
#[structopt(name = "event-sync", about = "Sync events between calendars")]
struct Args {
    #[structopt(flatten)]
    auth: Auth,

    #[structopt(subcommand)]
    cmd: Cmd,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::new()
        .filter_level(LevelFilter::Info)
        .parse_default_env()
        .init();

    let args = Args::from_args();

    let authenticator = args
        .auth
        .authenticator()
        .await
        .wrap_err("Failed to get authenticator")?;

    let https = hyper_rustls::HttpsConnector::with_native_roots();
    let client = hyper::Client::builder().build(https);

    let hub = CalendarHub::new(client, authenticator);

    match args.cmd {
        Cmd::List(list) => list.run(hub).await?,
    }

    Ok(())
}
