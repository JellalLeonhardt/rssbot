use std::collections::HashMap;
use std::time::Duration;

use futures::prelude::*;
use regex::Regex;
use telebot;
use telebot::functions::*;
use telebot::objects::ResponseParameters;
use tokio_core::reactor::{Interval, Timeout};
use tokio_curl::Session;

use data;
use feed;
use utlis::{chat_is_unavailable, format_and_split_msgs, gen_ua, send_multiple_messages,
            to_chinese_error_msg, truncate_message, Escape, EscapeUrl, TELEGRAM_MAX_MSG_LEN};

lazy_static!{
    // it's different from `feed::HOST`, so maybe need a better name?
    static ref HOST: Regex = Regex::new(r"^(?:https?://)?([^/]+)").unwrap();
}

pub fn spawn_fetcher(bot: telebot::RcBot, db: data::Database, period: u64) {
    let handle = bot.inner.handle.clone();
    let handle2 = handle.clone();
    let lop = async_block! {
        #[async]
        for _ in Interval::new(Duration::from_secs(period), &handle)
            .expect("failed to start feed loop")
            .map_err(|e| error!("feed loop error: {}", e))
        {
            let feeds = db.get_all_feeds();
            let grouped_feeds = grouping_by_host(feeds);
            let handle2 = handle.clone();
            let bot = bot.clone();
            let db = db.clone();
            let fetcher = async_block! {
                for group in grouped_feeds {
                    let session = Session::new(handle2.clone());
                    let bot = bot.clone();
                    let db = db.clone();
                    let group_fetcher = async_block! {
                        for feed in group {
                            await!(fetch_feed_updates(bot.clone(), db.clone(),
                                                      session.clone(), feed))?;
                        }
                        Ok(())
                    };
                    handle2.spawn(group_fetcher);
                    await!(Timeout::new(Duration::from_secs(1), &handle2)
                           .expect("failed to start sleep"))
                        .map_err(|e| error!("feed loop sleep error: {}", e))?;
                }
                Ok(())
            };
            handle.spawn(fetcher);
        }
        Ok(())
    };
    handle2.spawn(lop)
}

fn grouping_by_host(feeds: Vec<data::Feed>) -> Vec<Vec<data::Feed>> {
    let mut result = HashMap::new();
    for feed in feeds {
        let host = get_host(&feed.link).to_owned();
        let group = result.entry(host).or_insert_with(Vec::new);
        group.push(feed);
    }
    result.into_iter().map(|(_, v)| v).collect()
}

fn get_host(url: &str) -> &str {
    HOST.captures(url)
        .map_or(url, |r| r.get(0).unwrap().as_str())
}

#[async]
fn fetch_feed_updates(
    bot: telebot::RcBot,
    db: data::Database,
    session: Session,
    feed: data::Feed,
) -> Result<(), ()> {
    let handle = bot.inner.handle.clone();
    let rss = match await!(feed::fetch_feed(
        session,
        gen_ua(&bot),
        feed.link.to_owned(),
    )) {
        Ok(rss) => rss,
        Err(e) => {
            // 1440 * 5 minute = 5 days
            if db.inc_error_count(&feed.link) > 1440 {
                db.reset_error_count(&feed.link);
                let err_msg = to_chinese_error_msg(e);
                let msg = format!(
                    "《<a href=\"{}\">{}</a>》\
                     已经连续 5 天拉取出错 ({}),\
                     可能已经关闭, 请取消订阅",
                    EscapeUrl(&feed.link),
                    Escape(&feed.title),
                    Escape(&err_msg)
                );
                for subscriber in feed.subscribers {
                    let m = bot.message(subscriber, msg.clone())
                        .parse_mode("HTML")
                        .disable_web_page_preview(true)
                        .send();
                    match await!(m) {
                        Err(telebot::Error::Telegram(_, ref s, None)) if chat_is_unavailable(s) => {
                            db.delete_subscriber(subscriber);
                        }
                        Err(telebot::Error::Telegram(
                            _,
                            _,
                            Some(ResponseParameters {
                                migrate_to_chat_id: Some(new_id),
                                ..
                            }),
                        )) => {
                            db.update_subscriber(subscriber, new_id);
                            handle.spawn(
                                bot.message(new_id, msg.clone())
                                    .parse_mode("HTML")
                                    .disable_web_page_preview(true)
                                    .send()
                                    .then(|_| Ok(())),
                            );
                        }
                        Err(e) => warn!("failed to send error to {}, {:?}", subscriber, e),
                        _ => (),
                    }
                }
            }
            return Ok(());
        }
    };
    let moved = if rss.source.as_ref().unwrap() != &feed.link {
        Some(rss.clone())
    } else {
        None
    };
    if rss.title != feed.title {
        db.update_title(&feed.link, &rss.title);
    }
    let feed::RSS {
        title: rss_title,
        link: rss_link,
        items: rss_items,
        ..
    } = rss;
    let updates = db.update(&feed.link, rss_items);
    if updates.is_empty() {
        return Ok(());
    }

    let msgs = format_and_split_msgs(
        format!("<b>{}</b>", Escape(&rss_title)),
        &updates,
        move |item| {
            let title = item.title
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or_else(|| &rss_title);
            let link = item.link
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or_else(|| &rss_link);
            format!(
                "{}\n{}",
                Escape(&truncate_message(title, TELEGRAM_MAX_MSG_LEN - 500)),
                EscapeUrl(link)
            )
        },
    );

    for subscriber in feed.subscribers {
        let r = send_multiple_messages(&bot, subscriber, msgs.clone());
        match await!(r) {
            Err(telebot::Error::Telegram(_, ref s, None)) if chat_is_unavailable(s) => {
                db.delete_subscriber(subscriber);
            }
            Err(telebot::Error::Telegram(
                _,
                _,
                Some(ResponseParameters {
                    migrate_to_chat_id: Some(new_id),
                    ..
                }),
            )) => {
                db.update_subscriber(subscriber, new_id);
                handle.spawn(send_multiple_messages(&bot, new_id, msgs.clone()).then(|_| Ok(())));
            }
            Err(e) => warn!("failed to send updates to {}, {:?}", subscriber, e),
            _ => (),
        }
        if let Some(ref rss) = moved {
            // ignore error
            let _ = db.unsubscribe(subscriber, &feed.link);
            let _ = db.subscribe(subscriber, rss.source.as_ref().unwrap(), rss);
        }
    }
    Ok(())
}
