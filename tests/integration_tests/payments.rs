use crate::common::{retry, test_context::TestContext, MockBankAction};
use retry_policies::policies::ExponentialBackoff;
use std::{collections::HashMap, time::Duration};
use test_case::test_case;
use truelayer_rust::{
    apis::{
        merchant_accounts::ListPaymentSourcesRequest,
        payments::{
            AuthorizationFlow, AuthorizationFlowActions, AuthorizationFlowNextAction,
            AuthorizationFlowResponseStatus, Beneficiary, CreatePaymentRequest,
            CreatePaymentUserRequest, Currency, FailureStage, PaymentMethod, PaymentStatus,
            ProviderSelection, ProviderSelectionSupported, RedirectSupported,
            StartAuthorizationFlowRequest, StartAuthorizationFlowResponse,
            SubmitProviderSelectionActionRequest,
        },
        payouts::{CreatePayoutRequest, PayoutBeneficiary, PayoutStatus},
    },
    pollable::PollOptions,
    PollableUntilTerminalState,
};
use url::Url;
use uuid::Uuid;

static MOCK_PROVIDER_ID: &str = "mock-payments-gb-redirect";
static MOCK_RETURN_URI: &str = "http://localhost:3000/callback";

#[tokio::test]
async fn fetch_non_existing_payment_returns_none() {
    let ctx = TestContext::start().await;

    let payment = ctx
        .client
        .payments
        .get_by_id(&Uuid::new_v4().to_string())
        .await
        .unwrap();

    assert!(payment.is_none());
}

#[tokio::test]
async fn hpp_link_returns_200() {
    let ctx = TestContext::start().await;

    // Create a payment
    let res = ctx
        .client
        .payments
        .create(&CreatePaymentRequest {
            amount_in_minor: 1,
            currency: Currency::Gbp,
            payment_method: PaymentMethod::BankTransfer {
                provider_selection: ProviderSelection::UserSelected { filter: None },
                beneficiary: Beneficiary::MerchantAccount {
                    merchant_account_id: ctx.merchant_account_gbp_id.clone(),
                    account_holder_name: None,
                },
            },
            user: CreatePaymentUserRequest::NewUser {
                name: Some("someone".to_string()),
                email: Some("some.one@email.com".to_string()),
                phone: None,
            },
            metadata: None,
        })
        .await
        .unwrap();

    // Get the HPP link for it
    let hpp_url = ctx
        .client
        .payments
        .get_hosted_payments_page_link(&res.id, &res.resource_token, MOCK_RETURN_URI)
        .await;

    // Make a request and assert we get back a 200
    assert!(reqwest::Client::new()
        .get(hpp_url)
        .header(
            reqwest::header::USER_AGENT,
            concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"))
        )
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ScenarioProviderSelection {
    UserSelected,
    Preselected,
}

enum ScenarioExpectedStatus {
    ExecutedOrSettled,
    Failed {
        failure_stage: FailureStage,
        failure_reason: &'static str,
    },
}

struct CreatePaymentScenario {
    provider_selection: ScenarioProviderSelection,
    mock_bank_action: MockBankAction,
    expected_status: ScenarioExpectedStatus,
    make_closed_loop_payout: bool,
}

impl CreatePaymentScenario {
    async fn run(&self) {
        let ctx = TestContext::start().await;

        // Create a payment
        let res = ctx
            .client
            .payments
            .create(&CreatePaymentRequest {
                amount_in_minor: 1,
                currency: Currency::Gbp,
                payment_method: PaymentMethod::BankTransfer {
                    provider_selection: match self.provider_selection {
                        ScenarioProviderSelection::UserSelected => {
                            ProviderSelection::UserSelected { filter: None }
                        }
                        ScenarioProviderSelection::Preselected => ProviderSelection::Preselected {
                            provider_id: MOCK_PROVIDER_ID.to_string(),
                            scheme_id: "faster_payments_service".to_string(),
                            remitter: None,
                        },
                    },
                    beneficiary: Beneficiary::MerchantAccount {
                        merchant_account_id: ctx.merchant_account_gbp_id.clone(),
                        account_holder_name: None,
                    },
                },
                user: CreatePaymentUserRequest::NewUser {
                    name: Some("someone".to_string()),
                    email: Some("some.one@email.com".to_string()),
                    phone: None,
                },
                metadata: Some({
                    let mut map = HashMap::new();
                    map.insert("some".into(), "metadata".into());
                    map
                }),
            })
            .await
            .unwrap();

        // Assert that we got sensible values back
        assert!(!res.id.is_empty());
        assert!(!res.resource_token.expose_secret().is_empty());
        assert!(!res.user.id.is_empty());

        // Fetch the same payment
        let payment = ctx
            .client
            .payments
            .get_by_id(&res.id)
            .await
            .unwrap()
            .unwrap();

        // Ensure the returned payment contains correct data
        assert_eq!(payment.id, res.id);
        assert_eq!(payment.amount_in_minor, 1);
        assert_eq!(payment.currency, Currency::Gbp);
        assert_eq!(payment.user.id, res.user.id);
        assert_eq!(payment.user.name.as_deref(), Some("someone"));
        assert_eq!(payment.user.email.as_deref(), Some("some.one@email.com"));
        assert_eq!(payment.user.phone, None);
        assert!(matches!(
            payment.payment_method,
            PaymentMethod::BankTransfer {
                beneficiary: Beneficiary::MerchantAccount {
                    merchant_account_id,
                    ..
                },
                ..
            }
            if merchant_account_id == ctx.merchant_account_gbp_id
        ));
        assert_eq!(payment.status, PaymentStatus::AuthorizationRequired);
        assert_eq!(
            payment.metadata.unwrap().get("some"),
            Some(&"metadata".into())
        );

        // Start authorization flow
        let StartAuthorizationFlowResponse {
            mut authorization_flow,
            mut status,
        } = ctx
            .client
            .payments
            .start_authorization_flow(
                &res.id,
                &StartAuthorizationFlowRequest {
                    provider_selection: Some(ProviderSelectionSupported {}),
                    redirect: Some(RedirectSupported {
                        return_uri: MOCK_RETURN_URI.to_string(),
                    }),
                },
            )
            .await
            .unwrap();

        if self.provider_selection == ScenarioProviderSelection::UserSelected {
            // Assert that the next action in the auth flow is ProviderSelection
            assert_eq!(status, AuthorizationFlowResponseStatus::Authorizing);
            assert!(matches!(
                authorization_flow,
                Some(AuthorizationFlow {
                    actions: Some(AuthorizationFlowActions {
                        next: AuthorizationFlowNextAction::ProviderSelection { providers, .. }
                    }),
                    ..
                })
                if !providers.is_empty()
            ));

            // Retrieve the payment by id and re-check its status
            assert!(matches!(
                ctx.client
                    .payments
                    .get_by_id(&res.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                PaymentStatus::Authorizing {
                    authorization_flow: AuthorizationFlow {
                        actions: Some(AuthorizationFlowActions {
                            next: AuthorizationFlowNextAction::ProviderSelection { providers, .. }
                        }),
                        ..
                    }
                }
                if !providers.is_empty()
            ));

            // Submit provider selection
            let submit_provider_selection_response = ctx
                .client
                .payments
                .submit_provider_selection(
                    &res.id,
                    &SubmitProviderSelectionActionRequest {
                        provider_id: MOCK_PROVIDER_ID.to_string(),
                    },
                )
                .await
                .unwrap();

            status = submit_provider_selection_response.status;
            authorization_flow = submit_provider_selection_response.authorization_flow;
        }

        // Assert that the next action in the auth flow is Redirect
        assert_eq!(status, AuthorizationFlowResponseStatus::Authorizing);
        assert!(matches!(
            authorization_flow,
            Some(AuthorizationFlow {
                actions: Some(AuthorizationFlowActions {
                    next: AuthorizationFlowNextAction::Redirect { uri, .. }
                }),
                ..
            })
            if Url::parse(&uri).is_ok()
        ));

        // Retrieve the payment by id and re-check its status.
        // Also extract the redirect uri because it will be needed to drive the payment to completion.
        let payment = ctx
            .client
            .payments
            .get_by_id(&res.id)
            .await
            .unwrap()
            .unwrap();
        let redirect_uri = match payment.status {
            PaymentStatus::Authorizing {
                authorization_flow:
                    AuthorizationFlow {
                        actions:
                            Some(AuthorizationFlowActions {
                                next: AuthorizationFlowNextAction::Redirect { ref uri, .. },
                            }),
                        ..
                    },
            } => Url::parse(uri).unwrap(),
            _ => panic!("Invalid payment state"),
        };

        // Drive the payment to completion (either success or failure)
        ctx.complete_mock_bank_redirect_authorization(&redirect_uri, self.mock_bank_action.clone())
            .await
            .unwrap();

        // Wait for the payment to reach a terminal state
        let payment = payment
            .poll_until_terminal_state(
                &ctx.client,
                PollOptions::default().with_retry_policy(
                    ExponentialBackoff::builder()
                        .build_with_total_retry_duration(Duration::from_secs(60)),
                ),
            )
            .await
            .unwrap();

        // Assert that the payment reached the expected state
        match &self.expected_status {
            ScenarioExpectedStatus::ExecutedOrSettled => {
                assert!(matches!(
                    payment.status,
                    PaymentStatus::Executed { .. } | PaymentStatus::Settled { .. }
                ));
            }
            ScenarioExpectedStatus::Failed {
                failure_stage: expected_failure_stage,
                failure_reason: expected_failure_reason,
            } => {
                assert!(matches!(
                    payment.status,
                    PaymentStatus::Failed {
                        ref failure_stage,
                        ref failure_reason,
                        ..
                    } if failure_stage == expected_failure_stage && failure_reason == expected_failure_reason
                ));
            }
        }

        // Test a closed loop payout for the payment we just created
        if self.make_closed_loop_payout {
            // Get the payment source for the user created by this payment
            let payment_source = retry(Duration::from_secs(60), || async {
                ctx.client
                    .merchant_accounts
                    .list_payment_sources(
                        &ctx.merchant_account_gbp_id,
                        &ListPaymentSourcesRequest {
                            user_id: res.user.id.clone(),
                        },
                    )
                    .await
                    .unwrap()
                    .first()
                    .cloned()
            })
            .await
            .expect("Payment source failed to appear");

            // Create a payout against this payment source
            let create_payout_response = ctx
                .client
                .payouts
                .create(&CreatePayoutRequest {
                    merchant_account_id: ctx.merchant_account_gbp_id.clone(),
                    amount_in_minor: 1,
                    currency: Currency::Gbp,
                    beneficiary: PayoutBeneficiary::PaymentSource {
                        user_id: res.user.id,
                        payment_source_id: payment_source.id,
                        reference: "rust-sdk-test".to_string(),
                    },
                })
                .await
                .unwrap();

            // Wait until the payout is executed
            let payout = create_payout_response
                .poll_until_terminal_state(
                    &ctx.client,
                    PollOptions::default().with_retry_policy(
                        ExponentialBackoff::builder()
                            .build_with_total_retry_duration(Duration::from_secs(60)),
                    ),
                )
                .await
                .unwrap();

            // Assert that it succeeded
            assert!(matches!(payout.status, PayoutStatus::Executed { .. }));
        }
    }
}

// Test all possible combinations of authorization outcomes
#[test_case(
    ScenarioProviderSelection::UserSelected,
    MockBankAction::Execute,
    ScenarioExpectedStatus::ExecutedOrSettled,
    false
    ; "user selected provider successful authorization"
)]
#[test_case(
    ScenarioProviderSelection::UserSelected,
    MockBankAction::RejectAuthorisation,
    ScenarioExpectedStatus::Failed { failure_stage: FailureStage::Authorizing, failure_reason: "authorization_failed" },
    false
    ; "user selected provider reject authorization"
)]
#[test_case(
    ScenarioProviderSelection::UserSelected,
    MockBankAction::RejectExecution,
    ScenarioExpectedStatus::Failed { failure_stage: FailureStage::Authorized, failure_reason: "provider_rejected" },
    false
    ; "user selected provider reject execution"
)]
#[test_case(
    ScenarioProviderSelection::UserSelected,
    MockBankAction::Cancel,
    ScenarioExpectedStatus::Failed { failure_stage: FailureStage::Authorizing, failure_reason: "canceled" },
    false
    ; "user selected provider canceled"
)]
#[test_case(
    ScenarioProviderSelection::Preselected,
    MockBankAction::Execute,
    ScenarioExpectedStatus::ExecutedOrSettled,
    false
    ; "preselected provider successful authorization"
)]
#[test_case(
    ScenarioProviderSelection::Preselected,
    MockBankAction::RejectAuthorisation,
    ScenarioExpectedStatus::Failed { failure_stage: FailureStage::Authorizing, failure_reason: "authorization_failed" },
    false
    ; "preselected provider reject authorization"
)]
#[test_case(
    ScenarioProviderSelection::Preselected,
    MockBankAction::RejectExecution,
    ScenarioExpectedStatus::Failed { failure_stage: FailureStage::Authorized, failure_reason: "provider_rejected" },
    false
    ; "preselected provider reject execution"
)]
#[test_case(
    ScenarioProviderSelection::Preselected,
    MockBankAction::Cancel,
    ScenarioExpectedStatus::Failed { failure_stage: FailureStage::Authorizing, failure_reason: "canceled" },
    false
    ; "preselected provider canceled"
)]
#[test_case(
    ScenarioProviderSelection::Preselected,
    MockBankAction::Execute,
    ScenarioExpectedStatus::ExecutedOrSettled,
    true
    ; "closed loop payout"
)]
#[tokio::test]
async fn create_payment_scenarios(
    provider_selection: ScenarioProviderSelection,
    mock_bank_action: MockBankAction,
    expected_status: ScenarioExpectedStatus,
    make_closed_loop_payout: bool,
) {
    CreatePaymentScenario {
        provider_selection,
        mock_bank_action,
        expected_status,
        make_closed_loop_payout,
    }
    .run()
    .await;
}
