// Copyright (c) 2022 Cloudflare, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause

use crate::{durable::state_get_or_default, int_err};
use daphne::messages::TransitionFailure;
use serde::{Deserialize, Serialize};
use std::convert::TryInto;
use worker::*;

pub(crate) fn durable_report_store_name(
    task_id_base64url: &str,
    window: u64,
    bucket: u64,
) -> String {
    format!(
        "/task/{}/window/{}/bucket/{}",
        task_id_base64url, window, bucket
    )
}

pub(crate) const DURABLE_REPORT_STORE_DELETE_ALL: &str = "/internal/do/report_store/delete_all";
pub(crate) const DURABLE_REPORT_STORE_GET_PENDING: &str = "/internal/do/report_store/get_pending";
pub(crate) const DURABLE_REPORT_STORE_PUT_PENDING: &str = "/internal/do/report_store/put_pending";
pub(crate) const DURABLE_REPORT_STORE_PUT_PROCESSED: &str =
    "/internal/do/report_store/put_processed";
pub(crate) const DURABLE_REPORT_STORE_MARK_COLLECTED: &str =
    "/internal/do/report_store/mark_collected";

#[derive(Deserialize, Serialize)]
pub(crate) struct ReportStoreGetPending {
    pub(crate) reports_requested: u64,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum ReportStoreResult {
    Ok,
    Err(TransitionFailure),
}

/// Durable Object (DO) for storing reports and report metadata.
///
/// The naming convention for instances of the [`ReportStore`] DO is as follows:
///
/// > /task/<task_id>/window/<window>/bucket/<bucket>
///
/// where `<task_id>` is a task ID, `<window>` is a batch window, and `<bucket>` is a non-negative
/// integer. A batch window is a UNIX timestamp (in seconds) truncated by the minimum batch
/// duration. The instance in which a report is stored is derived from the task ID and nonce of the
/// report itself.
#[durable_object]
pub struct ReportStore {
    #[allow(dead_code)]
    state: State,
}

impl ReportStore {
    async fn checked_process(&self, nonce_hex: &str) -> Result<ReportStoreResult> {
        let key = format!("processed/{}", nonce_hex);
        let collected: bool = state_get_or_default(&self.state, "collected").await?;
        let observed: bool = state_get_or_default(&self.state, &key).await?;
        if observed && !collected {
            return Ok(ReportStoreResult::Err(TransitionFailure::ReportReplayed));
        } else if !observed && collected {
            return Ok(ReportStoreResult::Err(TransitionFailure::BatchCollected));
        }
        self.state.storage().put(&key, true).await?;
        Ok(ReportStoreResult::Ok)
    }
}

#[durable_object]
impl DurableObject for ReportStore {
    fn new(state: State, _env: Env) -> Self {
        Self { state }
    }

    async fn fetch(&mut self, mut req: Request) -> Result<Response> {
        match (req.path().as_ref(), req.method()) {
            (DURABLE_REPORT_STORE_DELETE_ALL, Method::Post) => {
                self.state.storage().delete_all().await?;
                Response::from_json(&ReportStoreResult::Ok)
            }

            (DURABLE_REPORT_STORE_GET_PENDING, Method::Post) => {
                let info: ReportStoreGetPending = req.json().await?;
                let reports_requested = info.reports_requested.try_into().unwrap();
                let opt = ListOptions::new()
                    .prefix("pending/")
                    .limit(reports_requested);
                let iter = self.state.storage().list_with_options(opt).await?.entries();
                let mut item = iter.next()?;
                let mut reports = Vec::with_capacity(reports_requested);
                let mut keys = Vec::with_capacity(reports_requested);
                while !item.done() {
                    let (key, report_hex): (String, String) = item.value().into_serde()?;
                    reports.push(report_hex);
                    keys.push(key);
                    item = iter.next()?;
                }

                // NOTE In order to support DAP tasks that require longer batch lifetimes, it will
                // necessary to check if the lifetime has been reached before removing reports from
                // storage.
                self.state.storage().delete_multiple(keys).await?;
                Response::from_json(&reports)
            }

            (DURABLE_REPORT_STORE_PUT_PENDING, Method::Post) => {
                let report_hex: String = req.json().await?;
                let nonce_hex = nonce_hex_from_report(&report_hex)
                    .ok_or_else(|| int_err("failed to parse nonce from report"))?;

                let res = self.checked_process(nonce_hex).await?;
                if matches!(res, ReportStoreResult::Ok) {
                    let key = format!("pending/{}", nonce_hex);
                    self.state.storage().put(&key, report_hex).await?;
                }
                Response::from_json(&res)
            }

            (DURABLE_REPORT_STORE_PUT_PROCESSED, Method::Post) => {
                let nonce_hex: String = req.json().await?;
                Response::from_json(&self.checked_process(&nonce_hex).await?)
            }

            (DURABLE_REPORT_STORE_MARK_COLLECTED, Method::Post) => {
                self.state.storage().put("collected", true).await?;
                Response::from_json(&ReportStoreResult::Ok)
            }

            _ => Err(int_err(format!(
                "ReportStore: unexpected request: method={:?}; path={:?}",
                req.method(),
                req.path()
            ))),
        }
    }
}

pub(crate) fn nonce_hex_from_report(report_hex: &str) -> Option<&str> {
    // task_id
    if report_hex.len() < 64 {
        return None;
    }
    let report_hex = &report_hex[64..];

    // nonce
    if report_hex.len() < 48 {
        return None;
    }
    Some(&report_hex[..48])
}
