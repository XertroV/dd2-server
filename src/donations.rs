use chrono::NaiveDateTime;
use log::info;
use num_traits::cast::ToPrimitive;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::types::chrono::DateTime;
use sqlx::{query, types::BigDecimal, Pool, Postgres};
use std::str::FromStr;

use crate::router::Donation;

const MATCHERINO_URL: &str = "https://api.matcherino.com/__api/bounties/findById?id=111501";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DonationFull {
    pub id: i32,
    pub action: String,
    pub authProvider: String,
    pub userName: String,
    pub userId: i32,
    pub displayName: String,
    pub comment: String,
    pub amount: f64,
    pub avatar: String,
    pub createdAt: String,
    pub socialMediaIdentifier: String,
    pub charityContribution: bool,
    pub supercellBgcolor: String,
    pub supercellCharacter: String,
}

pub async fn get_donations_from_matcherino() -> Result<Vec<DonationFull>, Box<dyn std::error::Error>> {
    let resp = reqwest::get(MATCHERINO_URL).await?.json::<Value>().await?;
    let body = resp.get("body").ok_or("No body in response")?;
    let txs = body.get("transactions").ok_or("No transactions")?;
    let txs = txs.as_array().ok_or("Transactions not an array")?;
    let mut ret = vec![];
    for tx in txs {
        let donation: DonationFull = serde_json::from_value(tx.clone())?;
        ret.push(donation);
    }
    Ok(ret)
}

pub async fn update_donations_in_db(
    pool: &Pool<Postgres>,
    donos: &Vec<DonationFull>,
) -> Result<Vec<DonationFull>, Box<dyn std::error::Error>> {
    let mut ret = vec![];
    for dono in donos {
        let r = query!("SELECT EXISTS(SELECT 1 FROM donations WHERE id = $1);", dono.id)
            .fetch_one(pool)
            .await?;
        if r.exists.unwrap_or(false) {
            continue;
        }

        query!("INSERT INTO donations (id, auth_provider, user_name, display_name, comment, amount, avatar, donated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8);",
            dono.id,
            dono.authProvider.clone(),
            dono.userName.clone(),
            dono.displayName.clone(),
            dono.comment.clone(),
            BigDecimal::try_from(dono.amount / 100.)?,
            dono.avatar.clone(),
            Some(DateTime::parse_from_rfc3339(dono.createdAt.as_str())?.naive_utc()),
        ).execute(pool).await?;
        ret.push(dono.clone());
    }

    for dono in ret.iter() {
        log::info!("Inserted donation: {:?}: ${:?}", dono.id, dono.amount / 100.);
        let r = query!("INSERT INTO user_dono_totals (user_name, total) VALUES ($1, $2) ON CONFLICT (user_name) DO UPDATE SET total = user_dono_totals.total + $2 RETURNING total;",
            dono.userName,
            BigDecimal::try_from(dono.amount / 100.).unwrap(),
        ).fetch_one(pool).await?;
        info!("Total for {:?}: {:?}", dono.userName, r.total);
    }
    Ok(ret)
}

pub async fn get_donations_and_donors(pool: &Pool<Postgres>) -> Result<(Vec<Donation>, Vec<(String, f64)>), sqlx::Error> {
    let r = query!("SELECT display_name, amount, donated_at FROM donations ORDER BY donated_at DESC;")
        .fetch_all(pool)
        .await?;
    let donos: Vec<Donation> = r
        .into_iter()
        .map(|r| Donation {
            name: r.display_name,
            amount: r.amount.to_f64().unwrap_or(-1.),
            ts: r.donated_at.and_utc().timestamp(),
        })
        .collect();
    let totals = query!("SELECT user_name, total FROM user_dono_totals;").fetch_all(pool).await?;
    let totals: Vec<(String, f64)> = totals.into_iter().map(|r| (r.user_name, r.total.to_f64().unwrap())).collect();
    Ok((donos, totals))
}
