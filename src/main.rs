use std::collections::HashMap;

use chrono::{DateTime, FixedOffset, Utc};
use eyre::{eyre, Report, Result, WrapErr};
use futures::stream::{Stream, StreamExt, TryStreamExt};
use google_apis_common::Connector;
use google_calendar3::{api::{Event, EventDateTime, EventListCall}, CalendarHub};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
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
    async fn run<C>(&self, hub: CalendarHub<C>) -> Result<()> where C: Connector {
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

fn maybe_update_req<R, V>(req: R, val: Option<V>, f: impl FnOnce(R, V) -> R) -> R {
    if let Some(v) = val {
        f(req, v)
    } else {
        req
    }
}

fn list_events<'a, C>(
    hub: &'a CalendarHub<C>, calendar: &'a str,
    updated_after: Option<DateTime<Utc>>,
    time_min: Option<DateTime<Utc>>,
) -> impl 'a + Stream<Item = Result<Event, Report>> 
where
    C: Connector {
    page_iterator::stream_items(move |next_page_token| async move {
        let req = hub.events().list(calendar);
        let req = maybe_update_req(req, next_page_token, |req, token| req.page_token(token.as_str()));
        let req = maybe_update_req(req, updated_after, |req, updated_after| req.updated_min(updated_after));
        let req = maybe_update_req(req, time_min, |req, time_min| req.time_min(time_min));
        let (_body, response) = req.doit().await?;
        
        Ok::<_, Report>((response.items.unwrap_or(vec![]), response.next_page_token))
    }, |items| items)
}

impl List {
    async fn run<C>(&self, hub: CalendarHub<C>) -> Result<()> where C: Connector {
        let iter = list_events(&hub, self.calendar.as_str(), None, None);
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
    updated_after: Option<DateTime<FixedOffset>>,
    #[structopt(
        short = "a",
        long = "after",
        help = "Only events after this time",
    )]
    starting_after: Option<DateTime<FixedOffset>>,
    #[structopt(
        long = "colour-id",
        help = "Colour ID for created event",
    )]
    colour_id: Option<String>,
    #[structopt(
        long = "dry-run",
        help = "Don't actually create events",
    )]
    dry_run: bool,
}

const SRC_ID_KEY: &'static str = "event-sync-src-id";

impl Sync {
    async fn run<'a, C>(&self, hub: CalendarHub<C>) -> Result<()> where C: Connector {
        let hub1 = hub.clone();
        let hub2 = hub.clone();
        let updated_after = self.updated_after.map(|x| x.to_utc());
        let starting_after = self.starting_after.map(|x| x.to_utc());
        let src_events = list_events(&hub1, self.src.as_str(), updated_after.clone(), starting_after.clone());
        let dst_events = list_events(&hub2, self.dst.as_str(), updated_after.clone(), starting_after.clone());

        let dst_events_by_src_id = dst_events
            .try_filter_map(|event| async move {
                log::debug!("{}", serde_json::to_string(&event).unwrap());
                let src_id = event
                    .extended_properties
                    .as_ref()
                    .and_then(|x| x.shared.as_ref())
                    .and_then(|m| m.get(SRC_ID_KEY).cloned());

                match src_id {
                    None => Ok(None),
                    Some(id) => Ok(Some((id.clone(), event))),
                }
            })
            .try_collect::<HashMap<String, Event>>()
            .await?;

        let hub = &hub;

        let dst_events_by_src_id = &dst_events_by_src_id;
        src_events
            .try_for_each(|src_event| async move {
                // TODO: Handle updates
                let id = src_event.id.ok_or(eyre!("Event missing id"))?;
                let existing = dst_events_by_src_id.get(&id);
                match existing {
                    Some(existing) => {
                        log::info!("Ignoring {} because a matching event already exists: {:?}", id, existing.id);
                        Ok(())
                    }
                    None => {
                        let mut properties = src_event.extended_properties.clone();
                        let props = properties.get_or_insert_with(|| Default::default());
                        let shared = props.shared.get_or_insert_with(|| Default::default());
                        shared.insert(SRC_ID_KEY.to_string(), id.clone());

                        let dst_event = Event {
                            summary: src_event.summary.clone(),
                            location: src_event.location.clone(),
                            start: src_event.start.clone(),
                            end: src_event.end.clone(),
                            extended_properties: properties,
                            color_id: self.colour_id.clone(),

                            ..Default::default()
                        };

                        log::info!("Inserting event for {}: {}", &id, serde_json::to_string(&dst_event).unwrap());
                        if !self.dry_run {
                            hub.events()
                                .insert(dst_event, &self.dst)
                                .add_scope(google_calendar3::api::Scope::Event)
                                .doit()
                                .await?;
                        }

                        Ok(())
                    }
                }
            })
            .await?;

        Ok(())
    }
}
#[derive(Debug, StructOpt)]
struct ImportEvent {
    #[structopt(
        short = "c",
        long = "calendar",
        help = "Calendar ID",
        default_value = "primary"
    )]
    calendar: String,
    #[structopt(
        short = "i",
        long = "ical-uid",
        help = "ID of event to import",
    )]
    ical_uid: String,
    #[structopt(
        long = "start",
        help = "Start time of event",
    )]
    start: DateTime<FixedOffset>,
    #[structopt(
        long = "end",
        help = "End time of event",
    )]
    end: DateTime<FixedOffset>,
}

impl ImportEvent {
    async fn run<C>(&self, hub: CalendarHub<C>) -> Result<()> where C: Connector {
        let event = Event {
            i_cal_uid: Some(self.ical_uid.clone()),
            start: Some(EventDateTime {
                date_time: Some(self.start.to_utc()),
                ..Default::default()
            }),
            end: Some(EventDateTime {
                date_time: Some(self.end.to_utc()),
                ..Default::default()
            }),
            ..Default::default()
        };

        hub
            .events()
            .import(event, &self.calendar)
            .add_scope(google_calendar3::api::Scope::Event)
            .doit()
            .await?;
        Ok(())
    }
}

#[derive(Debug, StructOpt)]
enum Cmd {
    List(List),
    ListCalendars(ListCalendars),
    Sync(Sync),
    ImportEvent(ImportEvent),
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
    let _ = rustls::crypto::ring::default_provider().install_default();

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

    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .unwrap()
        .https_or_http()
        .enable_http1()
        .build();
    let client = Client::builder(TokioExecutor::new()).build(https);

    let hub = CalendarHub::new(client, authenticator);

    match args.cmd {
        Cmd::List(list) => list.run(hub).await?,
        Cmd::ListCalendars(list) => list.run(hub).await?,
        Cmd::Sync(sync) => sync.run(hub).await?,
        Cmd::ImportEvent(import) => import.run(hub).await?,
    }

    Ok(())
}
