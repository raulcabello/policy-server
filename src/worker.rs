use anyhow::Result;
use itertools::Itertools;
use policy_evaluator::callback_requests::CallbackRequest;
use policy_evaluator::wasmtime;
use policy_evaluator::{
    admission_response::{AdmissionResponse, AdmissionResponseStatus},
    policy_evaluator::{PolicyEvaluator, ValidateRequest},
};
use std::{collections::HashMap, fmt, time::Instant};
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::{error, info, info_span};

use crate::communication::EvalRequest;
use crate::metrics;
use crate::settings::{Policy, PolicyMode};
use crate::worker_pool::PrecompiledPolicies;

struct PolicyEvaluatorWithSettings {
    policy_evaluator: PolicyEvaluator,
    policy_mode: PolicyMode,
    allowed_to_mutate: bool,
    always_accept_admission_reviews_on_namespace: Option<String>,
}

pub(crate) struct Worker {
    evaluators: HashMap<String, PolicyEvaluatorWithSettings>,
    channel_rx: Receiver<EvalRequest>,

    // TODO: remove clippy's exception. This is going to be used to
    // implement the epoch handling
    #[allow(dead_code)]
    engine: wasmtime::Engine,
}

pub struct PolicyErrors(HashMap<String, String>);

impl fmt::Display for PolicyErrors {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut errors = self
            .0
            .iter()
            .map(|(policy, error)| format!("[{}: {}]", policy, error));
        write!(f, "{}", errors.join(", "))
    }
}

impl Worker {
    #[tracing::instrument(
        name = "worker_new",
        fields(host=crate::cli::HOSTNAME.as_str()),
        skip_all,
    )]
    pub(crate) fn new(
        rx: Receiver<EvalRequest>,
        policies: &HashMap<String, Policy>,
        precompiled_policies: &PrecompiledPolicies,
        wasmtime_config: &wasmtime::Config,
        callback_handler_tx: Sender<CallbackRequest>,
        always_accept_admission_reviews_on_namespace: Option<String>,
    ) -> Result<Worker, PolicyErrors> {
        let mut evs_errors = HashMap::new();
        let mut evs = HashMap::new();

        let engine = wasmtime::Engine::new(wasmtime_config).map_err(|e| {
            let mut errors = HashMap::new();
            errors.insert(
                "*".to_string(),
                format!("Cannot create wasmtime::Engine: {:?}", e),
            );
            PolicyErrors(errors)
        })?;

        for (id, policy) in policies.iter() {
            // It's safe to clone the outer engine. This creates a shallow copy
            let inner_engine = engine.clone();
            let policy_evaluator = match crate::worker_pool::build_policy_evaluator(
                id,
                policy,
                &inner_engine,
                precompiled_policies,
                callback_handler_tx.clone(),
            ) {
                Ok(pe) => pe,
                Err(e) => {
                    evs_errors.insert(
                        id.clone(),
                        format!("[{}] could not create PolicyEvaluator: {:?}", id, e),
                    );
                    continue;
                }
            };

            let policy_evaluator_with_settings = PolicyEvaluatorWithSettings {
                policy_evaluator,
                policy_mode: policy.policy_mode.clone(),
                allowed_to_mutate: policy.allowed_to_mutate.unwrap_or(false),
                always_accept_admission_reviews_on_namespace:
                    always_accept_admission_reviews_on_namespace.clone(),
            };

            evs.insert(id.to_string(), policy_evaluator_with_settings);
        }

        if !evs_errors.is_empty() {
            return Err(PolicyErrors(evs_errors));
        }

        Ok(Worker {
            evaluators: evs,
            channel_rx: rx,
            engine,
        })
    }

    // Returns a validation response with policy-server specific
    // constraints taken into account:
    // - A policy might have tried to mutate while the policy-server
    //   configuration does not allow it to mutate
    // - A policy might be running in "Monitor" mode, that always
    //   accepts the request (without mutation), logging the answer
    fn validation_response_with_constraints(
        policy_id: &str,
        policy_mode: &PolicyMode,
        allowed_to_mutate: bool,
        validation_response: AdmissionResponse,
    ) -> AdmissionResponse {
        match policy_mode {
            PolicyMode::Protect => {
                if validation_response.patch.is_some() && !allowed_to_mutate {
                    AdmissionResponse {
                        allowed: false,
                        status: Some(AdmissionResponseStatus {
                            message: Some(format!("Request rejected by policy {}. The policy attempted to mutate the request, but it is currently configured to not allow mutations.", policy_id)),
                            code: None,
                        }),
                        // if `allowed_to_mutate` is false, we are in a validating webhook. If we send a patch, k8s will fail because validating webhook do not expect this field
                        patch: None,
                        patch_type: None,
                        ..validation_response
                    }
                } else {
                    validation_response
                }
            }
            PolicyMode::Monitor => {
                // In monitor mode we always accept
                // the request, but log what would
                // have been the decision of the
                // policy. We also force mutating
                // patches to be none. Status is also
                // overriden, as it's only taken into
                // account when a request is rejected.
                info!(
                    policy_id = policy_id,
                    allowed_to_mutate = allowed_to_mutate,
                    response = format!("{:?}", validation_response).as_str(),
                    "policy evaluation (monitor mode)",
                );
                AdmissionResponse {
                    allowed: true,
                    patch_type: None,
                    patch: None,
                    status: None,
                    ..validation_response
                }
            }
        }
    }

    pub(crate) fn run(mut self) {
        while let Some(req) = self.channel_rx.blocking_recv() {
            let span = info_span!(parent: &req.parent_span, "policy_eval");
            let _enter = span.enter();

            let res = match self.evaluators.get_mut(&req.policy_id) {
                Some(PolicyEvaluatorWithSettings {
                    policy_evaluator,
                    policy_mode,
                    allowed_to_mutate,
                    always_accept_admission_reviews_on_namespace,
                }) => match serde_json::to_value(req.req.clone()) {
                    Ok(json) => {
                        let policy_name = policy_evaluator.policy.id.clone();
                        let policy_mode = policy_mode.clone();
                        let start_time = Instant::now();
                        let allowed_to_mutate = *allowed_to_mutate;
                        let vanilla_validation_response =
                            policy_evaluator.validate(ValidateRequest::new(json));
                        let policy_evaluation_duration = start_time.elapsed();
                        let error_code = if let Some(status) = &vanilla_validation_response.status {
                            status.code
                        } else {
                            None
                        };
                        let validation_response = Worker::validation_response_with_constraints(
                            &req.policy_id,
                            &policy_mode,
                            allowed_to_mutate,
                            vanilla_validation_response.clone(),
                        );
                        let validation_response =
                            // If the policy server is configured to
                            // always accept admission reviews on a
                            // given namespace, just set the `allowed`
                            // part of the response to `true` if the
                            // request matches this namespace. Keep
                            // the rest of the behaviors unchanged,
                            // such as checking if the policy is
                            // allowed to mutate.
                            if let Some(namespace) = always_accept_admission_reviews_on_namespace {
                                if req.req.namespace == Some(namespace.to_string()) {
                                    AdmissionResponse {
                                        allowed: true,
                                        ..validation_response
                                    }
                                } else {
                                    validation_response
                                }
                            } else {
                                validation_response
                            };
                        let accepted = vanilla_validation_response.allowed;
                        let mutated = vanilla_validation_response.patch.is_some();
                        let res = req.resp_chan.send(Some(validation_response));
                        let policy_evaluation = metrics::PolicyEvaluation {
                            policy_name,
                            policy_mode: policy_mode.into(),
                            resource_namespace: req.req.namespace,
                            resource_kind: req.req.request_kind.unwrap_or_default().kind,
                            resource_request_operation: req.req.operation.clone(),
                            accepted,
                            mutated,
                            error_code,
                        };
                        metrics::record_policy_latency(
                            policy_evaluation_duration,
                            &policy_evaluation,
                        );
                        metrics::add_policy_evaluation(&policy_evaluation);
                        res
                    }
                    Err(e) => {
                        let error_msg = format!("Failed to serialize AdmissionReview: {:?}", e);
                        error!("{}", error_msg);
                        req.resp_chan.send(Some(AdmissionResponse::reject(
                            req.policy_id,
                            error_msg,
                            warp::http::StatusCode::BAD_REQUEST.as_u16(),
                        )))
                    }
                },
                None => req.resp_chan.send(None),
            };
            if res.is_err() {
                error!("receiver dropped");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY_ID: &str = "policy-id";

    #[test]
    fn validation_response_with_constraints_not_allowed_to_mutate() {
        let rejection_response = AdmissionResponse {
            allowed: false,
            patch: None,
            patch_type: None,
            status: Some(AdmissionResponseStatus {
                message: Some("Request rejected by policy policy-id. The policy attempted to mutate the request, but it is currently configured to not allow mutations.".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let accept_response = AdmissionResponse {
            allowed: true,
            ..Default::default()
        };

        assert_eq!(
            Worker::validation_response_with_constraints(
                POLICY_ID,
                &PolicyMode::Protect,
                false,
                AdmissionResponse {
                    allowed: true,
                    patch: Some("patch".to_string()),
                    patch_type: Some("application/json-patch+json".to_string()),
                    ..Default::default()
                },
            ),
            rejection_response,
        );

        assert_eq!(
            Worker::validation_response_with_constraints(
                POLICY_ID,
                &PolicyMode::Monitor,
                false,
                AdmissionResponse {
                    allowed: true,
                    patch: Some("patch".to_string()),
                    patch_type: Some("application/json-patch+json".to_string()),
                    ..Default::default()
                },
            ),
            accept_response,
        );
    }

    #[test]
    fn validation_response_with_constraints_monitor_mode() {
        let admission_response = AdmissionResponse {
            allowed: true,
            ..Default::default()
        };

        assert_eq!(
            Worker::validation_response_with_constraints(
                POLICY_ID,
                &PolicyMode::Monitor,
                true,
                AdmissionResponse {
                    allowed: true,
                    patch: Some("patch".to_string()),
                    patch_type: Some("application/json-patch+json".to_string()),
                    ..Default::default()
                },
            ),
            admission_response,
            "Mutated request from a policy allowed to mutate should be accepted in monitor mode"
        );

        assert_eq!(
            Worker::validation_response_with_constraints(
                POLICY_ID,
                &PolicyMode::Monitor,
                false,
                AdmissionResponse {
                    allowed: true,
                    patch: Some("patch".to_string()),
                    patch_type: Some("application/json-patch+json".to_string()),
                    ..Default::default()
                },
            ),
            admission_response, "Mutated request from a policy not allowed to mutate should be accepted in monitor mode"
        );

        assert_eq!(
            Worker::validation_response_with_constraints(
                POLICY_ID,
                &PolicyMode::Monitor,
                true,
                AdmissionResponse {
                    allowed: true,
                    ..Default::default()
                },
            ),
            admission_response,
            "Accepted request from a policy allowed to mutate should be accepted in monitor mode"
        );

        assert_eq!(
            Worker::validation_response_with_constraints(
                POLICY_ID,
                &PolicyMode::Monitor,
                true,
                AdmissionResponse {
                    allowed: false,
                    status: Some(AdmissionResponseStatus {
                        message: Some("some rejection message".to_string()),
                        code: Some(500),
                    }),
                    ..Default::default()
                },
            ),
            admission_response, "Not accepted request from a policy allowed to mutate should be accepted in monitor mode"
        );

        assert_eq!(
            Worker::validation_response_with_constraints(
                POLICY_ID,
                &PolicyMode::Monitor,
                false,
                AdmissionResponse {
                    allowed: true,
                    ..Default::default()
                },
            ),
            admission_response, "Accepted request from a policy not allowed to mutate should be accepted in monitor mode"
        );

        assert_eq!(
            Worker::validation_response_with_constraints(
                POLICY_ID,
                &PolicyMode::Monitor,
                false,
                AdmissionResponse {
                    allowed: false,
                    status: Some(AdmissionResponseStatus {
                        message: Some("some rejection message".to_string()),
                        code: Some(500),
                    }),
                    ..Default::default()
                },
            ),
            admission_response, "Not accepted request from a policy not allowed to mutate should be accepted in monitor mode"
        );
    }

    #[test]
    fn validation_response_with_constraints_protect_mode() {
        let admission_response = AdmissionResponse {
            allowed: true,
            ..Default::default()
        };

        assert_eq!(
            Worker::validation_response_with_constraints(
                POLICY_ID,
                &PolicyMode::Protect,
                true,
                AdmissionResponse {
                    allowed: true,
                    patch: Some("patch".to_string()),
                    patch_type: Some("application/json-patch+json".to_string()),
                    ..Default::default()
                },
            ),
            AdmissionResponse {
                allowed: true,
                patch: Some("patch".to_string()),
                patch_type: Some("application/json-patch+json".to_string()),
                ..Default::default()
            },
            "Mutated request from a policy allowed to mutate should be accepted in protect mode"
        );

        assert_eq!(
            Worker::validation_response_with_constraints(
                POLICY_ID,
                &PolicyMode::Protect,
                false,
                AdmissionResponse {
                    allowed: true,
                    patch: Some("patch".to_string()),
                    patch_type: Some("application/json-patch+json".to_string()),
                    ..Default::default()
                },
            ),
            AdmissionResponse {
            allowed: false,
            patch: None,
            patch_type: None,
            status: Some(AdmissionResponseStatus {
                message: Some("Request rejected by policy policy-id. The policy attempted to mutate the request, but it is currently configured to not allow mutations.".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        },
            "Mutated request from a policy not allowed to mutate should be reject in protect mode"
        );

        assert_eq!(
            Worker::validation_response_with_constraints(
                POLICY_ID,
                &PolicyMode::Protect,
                true,
                AdmissionResponse {
                    allowed: true,
                    ..Default::default()
                },
            ),
            admission_response,
            "Accepted request from a policy allowed to mutate should be accepted in protect mode"
        );

        assert_eq!(
            Worker::validation_response_with_constraints(
                POLICY_ID,
                &PolicyMode::Protect,
                true,
                AdmissionResponse {
                    allowed: false,
                    status: Some(AdmissionResponseStatus {
                        message: Some("some rejection message".to_string()),
                        code: Some(500),
                    }),
                    ..Default::default()
                },
            ),
            AdmissionResponse {
                    allowed: false,
                    status: Some(AdmissionResponseStatus {
                        message: Some("some rejection message".to_string()),
                        code: Some(500),
                    }),
                    ..Default::default()
                }, "Not accepted request from a policy allowed to mutate should be rejected in protect mode"
        );

        assert_eq!(
            Worker::validation_response_with_constraints(
                POLICY_ID,
                &PolicyMode::Protect,
                false,
                AdmissionResponse {
                    allowed: true,
                    ..Default::default()
                },
            ),
            admission_response, "Accepted request from a policy not allowed to mutate should be accepted in protect mode"
        );

        assert_eq!(
            Worker::validation_response_with_constraints(
                POLICY_ID,
                &PolicyMode::Protect,
                false,
                AdmissionResponse {
                    allowed: false,
                    status: Some(AdmissionResponseStatus {
                        message: Some("some rejection message".to_string()),
                        code: Some(500),
                    }),
                    ..Default::default()
                },
            ),
            AdmissionResponse {
                    allowed: false,
                    status: Some(AdmissionResponseStatus {
                        message: Some("some rejection message".to_string()),
                        code: Some(500),
                    }),
                    ..Default::default()
                }, "Not accepted request from a policy not allowed to mutate should be rejected in protect mode"
        );
    }
}
