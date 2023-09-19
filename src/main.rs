use std::env;
use std::{fs::File, time::Duration};

use anyhow::Error;
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client,
};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

macro_rules! some_or {
    ($input:expr, $fallback:expr) => {
        match $input {
            Some(x) => x,
            None => $fallback,
        }
    };
}

#[derive(Clone)]
struct Config {
    pagespeed_api_key: String,
    mastodon_access_token: String,
    instance: String,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    pretty_env_logger::init();

    let config = Config {
        pagespeed_api_key: env::var("A11Y_PAGESPEED_API_KEY")
            .expect("A11Y_PAGESPEED_API_KEY missing"),
        mastodon_access_token: env::var("A11Y_MASTODON_ACCESS_TOKEN")
            .expect("A11Y_MASTODON_ACCESS_TOKEN missing"),
        instance: env::var("A11Y_MASTODON_INSTANCE").expect("A11Y_MASTODON_INSTANCE missing"),
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {}", config.mastodon_access_token)).unwrap(),
    );
    headers.insert(
        "User-Agent",
        HeaderValue::from_str(&format!(
            "mastodon-a11y-generator/{}",
            env!("CARGO_PKG_VERSION")
        ))
        .unwrap(),
    );

    let client = Client::builder()
        .use_rustls_tls()
        .default_headers(headers)
        .build()?;

    let pagespeed_client = Client::builder().use_rustls_tls().build()?;

    let client2 = client.clone();
    let config2 = config.clone();

    tokio::join!(
        async move {
            loop {
                log::info!("polling home timeline");
                if let Err(e) =
                    check_statuses(&config, pagespeed_client.clone(), client.clone()).await
                {
                    log::error!("polling home timeline crashed: {}", e);
                }
                log::info!("polling home timeline: done");
                sleep(Duration::from_secs(120)).await;
            }
        },
        async move {
            loop {
                log::info!("polling followers");
                if let Err(e) = check_followers(&config2, client2.clone()).await {
                    log::error!("polling followers crashed: {}", e);
                }
                log::info!("polling followers: done");
                sleep(Duration::from_secs(60)).await;
            }
        }
    );

    Ok(())
}

async fn check_followers(config: &Config, client: Client) -> Result<(), Error> {
    let mut min_id: String = if let Ok(f) = File::open("notification-cursor.json") {
        serde_json::from_reader(f)?
    } else {
        "1".to_owned()
    };

    let mut item_count = 0;

    #[derive(Debug, Deserialize)]
    struct Notification {
        id: String,
        account: Account,
    }

    loop {
        let url = format!(
            "{}/api/v1/notifications?limit=30&types[]=follow",
            config.instance
        );

        let page_result = client
            .get(url)
            .query(&[("min_id", &min_id)])
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<Notification>>()
            .await?;

        if page_result.is_empty() {
            break;
        }

        for item in &page_result {
            item_count += 1;

            log::info!(
                "following account id={} acct={}",
                item.account.id,
                item.account.acct
            );

            client
                .post(&format!(
                    "{}/api/v1/accounts/{}/follow",
                    config.instance, item.account.id
                ))
                .send()
                .await?
                .error_for_status()?;
        }

        min_id = page_result[0].id.clone();
    }

    let f = File::create("notification-cursor.json")?;
    serde_json::to_writer(f, &min_id)?;

    log::info!("processed {} notifications", item_count);
    Ok(())
}

async fn check_statuses(
    config: &Config,
    pagespeed_client: Client,
    client: Client,
) -> Result<(), Error> {
    let mut min_id: String = if let Ok(f) = File::open("timeline-cursor.json") {
        serde_json::from_reader(f)?
    } else {
        "1".to_owned()
    };

    let mut status_count = 0;

    loop {
        let url = format!("{}/api/v1/timelines/home?limit=40", config.instance);

        let page_result = client
            .get(url)
            .query(&[("min_id", &min_id)])
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<Status>>()
            .await?;

        if page_result.is_empty() {
            break;
        }

        for status in &page_result {
            status_count += 1;
            if let Err(e) =
                inspect_status(config, pagespeed_client.clone(), client.clone(), status).await
            {
                log::error!("failed to process status {}: {}", status.id, e);
            }
        }

        min_id = page_result[0].id.clone();
    }

    let f = File::create("timeline-cursor.json")?;
    serde_json::to_writer(f, &min_id)?;

    log::info!("processed {} statuses", status_count);

    Ok(())
}

async fn inspect_status(
    config: &Config,
    pagespeed_client: Client,
    client: Client,
    status: &Status,
) -> Result<(), Error> {
    let card = some_or!(&status.card, return Ok(()));
    let url = some_or!(&card.url, return Ok(()));

    if status.in_reply_to_id.is_some() {
        return Ok(());
    }

    if status.reblog.is_some() {
        return Ok(());
    }

    let mut report_raw_builder = pagespeed_client
        .get("https://www.googleapis.com/pagespeedonline/v5/runPagespeed?category=accessibility")
        .query(&[("url", url)]);

    if !config.pagespeed_api_key.is_empty() {
        report_raw_builder = report_raw_builder.query(&[("key", &config.pagespeed_api_key)]);
    }

    let report = report_raw_builder
        .send()
        .await?
        .error_for_status()?
        .json::<PagespeedReport>()
        .await?;

    let score = report.lighthouse_result.categories.accessibility.score;

    log::info!("{} -> {}", url, score);

    if score >= 0.8 {
        return Ok(());
    }

    #[derive(Serialize, Debug)]
    struct PostStatus {
        status: String,
        in_reply_to_id: String,
        visibility: &'static str,
    }

    client
        .post(&format!("{}/api/v1/statuses", config.instance))
        .header("Idempotency-Key", format!("a11y-v2-{}", status.id))
        .form(&PostStatus {
            status: format!(
                "@{} Chrome Lighthouse reports a a11y score of {} on this link, FYI.\n\n{}",
                status.account.acct, score, url
            ),
            in_reply_to_id: status.id.clone(),
            visibility: "direct",
        })
        .send()
        .await?
        .error_for_status()?;

    Ok(())
}

#[derive(Deserialize, Debug)]
struct Dummy {}

#[derive(Deserialize, Debug)]
struct Status {
    id: String,
    #[serde(default)]
    card: Option<StatusCard>,
    #[serde(default)]
    reblog: Option<Dummy>,
    #[serde(default)]
    in_reply_to_id: Option<String>,
    account: Account,
}

#[derive(Deserialize, Debug)]
struct StatusCard {
    #[serde(default)]
    url: Option<String>,
}

#[derive(Deserialize, Debug)]
struct Account {
    id: String,
    acct: String,
}

#[derive(Debug, Deserialize)]
struct PagespeedReport {
    #[serde(rename = "lighthouseResult")]
    lighthouse_result: LighthouseReport,
}

#[derive(Debug, Deserialize)]
struct LighthouseReport {
    categories: LighthouseReportCategories,
}

#[derive(Debug, Deserialize)]
struct LighthouseReportCategories {
    accessibility: LighthouseReportCategory,
}

#[derive(Debug, Deserialize)]
struct LighthouseReportCategory {
    score: f64,
}
