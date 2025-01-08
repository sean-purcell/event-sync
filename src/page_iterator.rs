use std::borrow::Borrow;
use std::future::Future;

use futures::{
    future::{BoxFuture, FutureExt},
    stream::{self, Stream},
};

enum State {
    Going(Option<String>),
    Done
}

pub fn stream_pages<'a, F, Fut, E, Page>(
    fetch_page: F,
) -> impl 'a + Stream<Item = Result<Page, E>>
where
    F: 'a + Fn(Option<String>) -> Fut + Send + Sync,
    Fut: Future<Output = Result<(Page, Option<String>), E>>,
    Page: Send + 'static
{
    stream::try_unfold((State::Going(None), fetch_page), move |(state, fetch_page)| {
        async move {
            match state {
                State::Done => Ok(None),
                State::Going(page_token) => {
                    let (page, next_token) = fetch_page(page_token).await?;
                    let new_state = match next_token {
                        Some(token) => State::Going(Some(token.clone())),
                        None => State::Done,
                    };
                    // Yield the page and update the state with the next token
                    Ok(Some((page, (new_state, fetch_page))))
                }
            }
        }
    })
}
