#[macro_use]
extern crate diesel;
#[macro_use]
extern crate diesel_migrations;

use chrono::{DateTime, Utc};
use eyre::{Report, Result, WrapErr};
use futures::stream::{self, Stream, StreamExt, TryStreamExt};
use google_calendar3::{api::Event, CalendarHub};
use log::LevelFilter;
use structopt::StructOpt;
use yup_oauth2::{
    authenticator::DefaultAuthenticator, noninteractive::NoninteractiveTokens,
    NoninteractiveAuthenticator,
};

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

#[derive(Debug, StructOpt)]
struct ListCalendars {
}

impl ListCalendars {
    async fn run(&self, hub: CalendarHub) -> Result<()> {
        let hub = &hub;
        let iter = page_iterator::stream_items(
            move |next_page_token| async move {
                let req = hub.calendar_list().list();
                let req = match next_page_token {
                    Some(token) => req.page_token(token.as_str()),
                    None => req,
                };
                let (_body, response) = req.doit().await?;
                
                Ok::<_, Report>((response.items.unwrap_or(vec![]), response.next_page_token))
            },
            |items| items);
        iter.try_for_each(|item| async move {
                println!("{}", serde_json::to_string(&item).unwrap());
                Ok(())
            })
            .await
            .wrap_err("Failed to list items")?;
        Ok(())
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
}

fn list_events(
    hub: &CalendarHub, calendar: &str, updated_after: Option<DateTime<Utc>>,
) -> impl Stream<Item = Result<Event, Report>> {
    page_iterator::stream_items(move |next_page_token| async move {
        let req = hub.events().list(calendar);
        let req = match next_page_token {
            Some(token) => req.page_token(token.as_str()),
            None => req,
        };
        let req = match updated_after {
            Some(updated_min) => req.updated_min(updated_min.to_rfc3339().as_str()),
            None => req,
        };
        let (_body, response) = req.doit().await?;
        
        Ok::<_, Report>((response.items.unwrap_or(vec![]), response.next_page_token))
    }, |items| items)
}

impl List {
    async fn run(&self, hub: CalendarHub) -> Result<()> {
        let iter = list_events(&hub, self.calendar.as_str(), None);
        iter.take(self.num)
            .try_for_each(|item| async move {
                println!("{}", serde_json::to_string(&item).unwrap());
                Ok(())
            })
            .await
            .wrap_err("Failed to list items")?;
        Ok(())
    }
}

#[derive(Debug, StructOpt)]
struct Sync {
    #[structopt(
        long = "src",
        help = "Source calendar ID",
    )]
    src: String,
    #[structopt(
        long = "dst",
        help = "Destination calendar ID",
    )]
    dst: String,
    #[structopt(
        short = "u",
        long = "updated-after",
        help = "Only events updated after this time",
    )]
    updated_after: DateTime<Utc>,
}

impl Sync {
    async fn run(&self, hub: CalendarHub) -> Result<()> {
        let src_events = list_events(&hub.clone(), self.src.as_str(), Some(self.updated_after.clone()));
        Ok(())
    }
}

#[derive(Debug, StructOpt)]
enum Cmd {
    List(List),
    ListCalendars(ListCalendars),
    Sync(Sync),
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
        Cmd::ListCalendars(list) => list.run(hub).await?,
        Cmd::Sync(sync) => sync.run(hub).await?,
    }

    Ok(())
}
