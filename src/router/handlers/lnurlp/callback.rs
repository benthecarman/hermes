use std::{str::FromStr, time::Duration};

use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use fedimint_client::oplog::UpdateStreamOrOutcome;
use fedimint_core::{core::OperationId, task::spawn, Amount};
use fedimint_ln_client::{LightningClientModule, LnReceiveState};
use fedimint_mint_client::{MintClientModule, OOBNotes};
use futures::StreamExt;
use nostr::secp256k1::XOnlyPublicKey;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{error, info};
use url::Url;
use xmpp::{parsers::message::MessageType, Jid};

use crate::{
    config::CONFIG,
    error::AppError,
    model::{
        invoice::{InvoiceBmc, InvoiceForCreate},
        nip05relays::Nip05RelaysBmc,
    },
    router::handlers::{nostr::Nip05Relays, NameOrPubkey},
    state::AppState,
    utils::{create_xmpp_client, empty_string_as_none},
};

use super::LnurlStatus;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LnurlCallbackParams {
    pub amount: u64, // User specified amount in MilliSatoshi
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub nonce: Option<String>, // Optional parameter used to prevent server response caching
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub comment: Option<String>, // Optional parameter to pass the LN WALLET user's comment to LN SERVICE
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub proofofpayer: Option<String>, // Optional ephemeral secp256k1 public key generated by payer
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LnurlCallbackSuccessAction {
    pub tag: String,
    pub message: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LnurlCallbackResponse {
    pub status: LnurlStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub pr: String, // BOLT11 invoice
    pub verify: Url,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success_action: Option<LnurlCallbackSuccessAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routes: Option<Vec<String>>,
}

const MIN_AMOUNT: u64 = 1000;

#[axum_macros::debug_handler]
pub async fn handle_callback(
    Path(username): Path<String>,
    Query(params): Query<LnurlCallbackParams>,
    State(state): State<AppState>,
) -> Result<Json<LnurlCallbackResponse>, AppError> {
    info!("callback called with username: {}", username);
    if params.amount < MIN_AMOUNT {
        return Err(AppError {
            error: anyhow::anyhow!("Amount < MIN_AMOUNT"),
            status: StatusCode::BAD_REQUEST,
        });
    }
    let nip05relays = Nip05RelaysBmc::get_by(&state.mm, NameOrPubkey::Name, &username).await?;

    let ln = state.fm.get_first_module::<LightningClientModule>();
    let (op_id, pr) = ln
        .create_bolt11_invoice(
            Amount {
                msats: params.amount,
            },
            "test invoice".to_string(),
            None,
            (),
        )
        .await?;

    // insert invoice into db for later verification
    let id = InvoiceBmc::create(
        &state.mm,
        InvoiceForCreate {
            op_id: op_id.to_string(),
            amount: params.amount as i64,
            bolt11: pr.to_string(),
        },
    )
    .await?;

    // create subscription to operation
    let subscription = ln
        .subscribe_ln_receive(op_id)
        .await
        .expect("subscribing to a just created operation can't fail");

    spawn_invoice_subscription(state, id, nip05relays, subscription).await;

    let verify_url = format!(
        "http://{}:{}/lnurlp/{}/verify/{}",
        CONFIG.domain,
        CONFIG.port,
        username,
        op_id.to_string()
    );

    let res = LnurlCallbackResponse {
        pr: pr.to_string(),
        success_action: None,
        status: LnurlStatus::Ok,
        reason: None,
        verify: verify_url.parse()?,
        routes: None,
    };

    Ok(Json(res))
}

async fn spawn_invoice_subscription(
    state: AppState,
    id: i32,
    nip05relays: Nip05Relays,
    subscription: UpdateStreamOrOutcome<LnReceiveState>,
) {
    spawn("waiting for invoice being paid", async move {
        let mut stream = subscription.into_stream();
        while let Some(op_state) = stream.next().await {
            match op_state {
                LnReceiveState::Canceled { reason } => {
                    error!("Payment canceled, reason: {:?}", reason);
                    break;
                }
                LnReceiveState::Claimed => {
                    info!("Payment claimed");
                    let invoice = InvoiceBmc::settle(&state.mm, id)
                        .await
                        .expect("settling invoice can't fail");
                    notify_user(state, invoice.amount as u64, nip05relays.clone())
                        .await
                        .expect("notifying user can't fail");
                    break;
                }
                _ => {}
            }
        }
    });
}

async fn notify_user(
    state: AppState,
    amount: u64,
    nip05relays: Nip05Relays,
) -> Result<(), Box<dyn std::error::Error>> {
    let mint = state.fm.get_first_module::<MintClientModule>();
    let (operation_id, notes) = mint
        .spend_notes(Amount::from_msats(amount), Duration::from_secs(604800), ())
        .await?;
    send_nostr_dm(&state, &nip05relays, operation_id, amount, notes).await?;
    Ok(())
}

async fn send_nostr_dm(
    state: &AppState,
    nip05relays: &Nip05Relays,
    operation_id: OperationId,
    amount: u64,
    notes: OOBNotes,
) -> Result<()> {
    state
        .nostr
        .send_direct_msg(
            XOnlyPublicKey::from_str(&nip05relays.pubkey).unwrap(),
            json!({
                "operationId": operation_id,
                "amount": amount,
                "notes": notes.to_string(),
            })
            .to_string(),
            None,
        )
        .await?;
    Ok(())
}

// TODO: add xmpp to registration
async fn send_xmpp_msg(
    nip05relays: &Nip05Relays,
    operation_id: OperationId,
    amount: u64,
    notes: OOBNotes,
) -> Result<()> {
    let mut xmpp_client = create_xmpp_client()?;
    let recipient =
        xmpp::BareJid::new(&format!("{}@{}", nip05relays.name, CONFIG.xmpp_chat_server))?;

    xmpp_client
        .send_message(
            Jid::Bare(recipient),
            MessageType::Chat,
            "en",
            &json!({
                "operationId": operation_id,
                "amount": amount,
                "notes": notes.to_string(),
            })
            .to_string(),
        )
        .await;

    Ok(())
}
