#[macro_use]
extern crate diesel;
#[macro_use]
extern crate diesel_migrations;

use eyre::{Report, Result, WrapErr};
use futures::stream::{self, StreamExt, TryStreamExt};
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

impl List {
    async fn run(&self, hub: CalendarHub) -> Result<()> {
        let hub = &hub;
        let calendar = self.calendar.as_str();
        let iter = page_iterator::stream_items(move |next_page_token| async move {
            let req = hub.events().list(calendar);
            let req = match next_page_token {
                Some(token) => req.page_token(token.as_str()),
                None => req,
            };
            let (_body, response) = req.doit().await?;
            
            Ok::<_, Report>((response.items.unwrap_or(vec![]), response.next_page_token))
        }, |items| items);
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
enum Cmd {
    List(List),
    ListCalendars(ListCalendars),
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
    }

    Ok(())
}
