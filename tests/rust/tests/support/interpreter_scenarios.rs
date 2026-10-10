// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use super::*;

fn scalar_42(event: &ExecutionEvent) -> bool {
    event.results.as_ref().is_some_and(|results| {
        results
            .get("text/plain")
            .or_else(|| results.get("text"))
            .is_some_and(|value| value == "42")
    })
}

#[test]
fn scalar_result_accepts_execd_text_alias_without_relaxing_value() {
    for field in ["text", "text/plain"] {
        let event: ExecutionEvent =
            serde_json::from_value(serde_json::json!({"type":"result","results":{field:"42"}}))
                .unwrap();
        assert!(scalar_42(&event));
        let trace = diagnostics::raw_trace(
            200,
            None,
            serde_json::to_string(&event).unwrap().as_bytes(),
            false,
        );
        assert!(trace["frames"][0]["raw_redacted"]
            .as_str()
            .unwrap()
            .contains(field));
    }
    let wrong: ExecutionEvent =
        serde_json::from_value(serde_json::json!({"type":"result","results":{"text":"41"}}))
            .unwrap();
    assert!(!scalar_42(&wrong));
}

#[test]
fn concurrent_busy_requires_legal_status_json_or_typed_terminal_error() {
    let busy = br#"{"code":"RUNTIME_ERROR","message":"error running codes session is busy"}"#;
    assert!(check_busy_response(reqwest::StatusCode::INTERNAL_SERVER_ERROR, busy, true).is_ok());
    assert!(check_busy_response(reqwest::StatusCode::CONFLICT, busy, true).is_ok());
    assert!(check_busy_response(reqwest::StatusCode::OK, busy, true).is_err());
    assert!(check_busy_response(reqwest::StatusCode::INTERNAL_SERVER_ERROR, busy, false).is_err());
    let malformed = b"data: {\"type\":\"init\"}\n\ndata: {\"code\":\"RUNTIME_ERROR\",\"message\":\"error running codes session is busy\"}\n\n";
    assert!(check_busy_response(reqwest::StatusCode::OK, malformed, false).is_err());
    let trace = diagnostics::raw_trace(200, Some("text/event-stream"), malformed, false);
    assert!(trace["frames"][1]["raw_redacted"]
        .as_str()
        .unwrap()
        .contains("error running codes session is busy"));
    let typed = b"data: {\"type\":\"error\",\"error\":{\"ename\":\"RUNTIME_ERROR\",\"evalue\":\"session is busy\"}}\n\n";
    assert!(check_busy_response(reqwest::StatusCode::OK, typed, false).is_ok());
    for error in [
        serde_json::json!({}),
        serde_json::json!({"ename":"ValueError","evalue":"session is busy"}),
        serde_json::json!({"ename":"RUNTIME_ERROR"}),
        serde_json::json!({"ename":"RUNTIME_ERROR","evalue":"private arbitrary message"}),
    ] {
        let frame = format!(
            "data: {}\n\n",
            serde_json::json!({"type":"error","error":error})
        );
        assert!(check_busy_response(reqwest::StatusCode::OK, frame.as_bytes(), false).is_err());
    }
}

fn check_busy_response(status: reqwest::StatusCode, bytes: &[u8], json_mime: bool) -> Result {
    if matches!(status.as_u16(), 409 | 500) && json_mime {
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|_| "busy rejection JSON decoding failed")?;
        return require(
            diagnostics::innocuous_busy(&value),
            "unexpected busy rejection classification",
        );
    }
    require(status.is_success(), "unexpected busy rejection status")?;
    let events = parse_wire_events(bytes)?;
    require(
        events.last().is_some_and(|event| {
            event.event_type == "error"
                && event.error.as_ref().is_some_and(|error| {
                    error.ename == "RUNTIME_ERROR"
                        && matches!(
                            error.evalue.as_str(),
                            "session is busy" | "error running codes session is busy"
                        )
                })
        }),
        "busy response missing typed terminal error",
    )
}

async fn concurrent_busy(
    sandbox: &Sandbox,
    codes: &opensandbox::code_execution::Codes,
    id: &str,
    options: RunCodeOptions,
) -> Result {
    // One 30-second deadline encloses stream acquisition, marker, concurrent
    // request and original-execution cleanup. Reserve the last 10s for cleanup.
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_secs(30);
    let mut stream = tokio::time::timeout_at(
        started + Duration::from_secs(10),
        codes.run_stream(
            "import time\nprint('busy-start', flush=True)\ntime.sleep(60)",
            options,
        ),
    )
    .await
    .map_err(|_| "busy setup execution request timed out")?
    .map_err(|_| "busy setup execution stream failed")?;
    let outcome = tokio::time::timeout_at(started + Duration::from_secs(20), async {
        loop {
            let event = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx))
                .await
                .ok_or("busy setup stream ended before stdout marker")?
                .map_err(|_| "busy setup event decoding failed")?;
            require(
                !matches!(event.event_type.as_str(), "error" | "execution_complete"),
                "busy setup completed before concurrent request",
            )?;
            if event.event_type == "stdout"
                && event
                    .text
                    .as_deref()
                    .is_some_and(|text| text.contains("busy-start"))
            {
                break;
            }
        }
        let (status, bytes, json_mime) =
            interpreter_raw_response(sandbox, id, "42", "wire-concurrent-busy").await?;
        check_busy_response(status, &bytes, json_mime)
    })
    .await
    .unwrap_or_else(|_| Err("concurrent busy request timed out".into()));
    // Always interrupt the one original execution, even when its marker or the
    // second response failed. Drain before deleting the owned context.
    let cleanup: Result = tokio::time::timeout_at(deadline, async {
        codes
            .interrupt(id)
            .await
            .map_err(|_| "busy setup interrupt failed")?;
        let mut terminal = false;
        while let Some(event) = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
            let event = event.map_err(|_| "busy setup cleanup stream failed")?;
            terminal |= matches!(event.event_type.as_str(), "error" | "execution_complete");
            if event.event_type == "execution_complete" {
                return Ok(());
            }
        }
        require(terminal, "busy setup cleanup missing terminal event")
    })
    .await
    .unwrap_or_else(|_| Err("busy setup cleanup timed out".into()));
    match (outcome, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (result, Err(error)) => {
            Err(format!("busy scenario={result:?}; original execution cleanup={error}").into())
        }
    }
}

pub(super) async fn recorded_run(
    codes: &opensandbox::code_execution::Codes,
    code: &str,
    options: RunCodeOptions,
) -> Result<Execution> {
    let outcome = codes.run(code, options).await;
    let summary = diagnostics::sdk_summary(&outcome);
    diagnostics::save("sdk-code", &summary)?;
    outcome.map_err(|_| {
        format!(
            "interpreter execution failed ({})",
            summary["classification"]
        )
        .into()
    })
}
pub(super) async fn finish_context(
    codes: &opensandbox::code_execution::Codes,
    id: &str,
    outcome: Result,
) -> Result {
    let cleanup = codes.delete_context(id).await;
    match (outcome, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (result, Err(_)) => {
            Err(format!("interpreter scenario={result:?}; context cleanup failed").into())
        }
    }
}

// Wait for the execution's own stdout marker before interrupting. This is not a
// sleep/retry and never reissues code. A missing marker or terminal event fails.
pub(super) async fn interrupted_execution(
    codes: &opensandbox::code_execution::Codes,
    id: &str,
    options: RunCodeOptions,
    initialize_state: bool,
) -> Result<Execution> {
    let outcome = tokio::time::timeout(Duration::from_secs(20), async {
        let code = if initialize_state {
            "answer = 41\nimport time\nprint('interrupt-start', flush=True)\ntime.sleep(60)"
        } else {
            "import time\nprint('interrupt-start', flush=True)\ntime.sleep(60)"
        };
        let mut stream = codes.run_stream(code, options).await?;
        let mut execution = Execution::default();
        let mut interrupted = false;
        while let Some(event) = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
            let event = event?;
            let started = event.event_type == "stdout"
                && event
                    .text
                    .as_deref()
                    .is_some_and(|text| text.contains("interrupt-start"));
            let complete = event.event_type == "execution_complete";
            if event.event_type == "error" {
                execution.error = event.error;
            }
            if event.event_type == "execution_complete" {
                execution.complete = true;
            }
            if started && !interrupted {
                codes.interrupt(id).await?;
                interrupted = true;
            }
            if complete {
                break;
            }
        }
        if !interrupted || execution.error.is_none() {
            return Err(Error::Protocol(
                "interrupt did not yield kernel error after stdout marker".into(),
            ));
        }
        Ok(execution)
    })
    .await;
    match outcome {
        Ok(outcome) => {
            let mut summary = diagnostics::sdk_summary(&outcome);
            summary["source"] = serde_json::json!("sdk_stream_no_wire_capture");
            for key in ["stdout_count", "stderr_count", "result_count"] {
                summary.as_object_mut().unwrap().remove(key);
            }
            diagnostics::save("sdk-interruption", &summary)?;
            outcome.map_err(|_| "interpreter interruption stream failed".into())
        }
        Err(_) => {
            diagnostics::save(
                "sdk-interruption",
                &serde_json::json!({"source":"sdk_stream_no_wire_capture","classification":"interrupt_timeout"}),
            )?;
            Err("interpreter interruption timed out".into())
        }
    }
}

#[derive(Clone, Copy)]
enum InterpreterPhase {
    State,
    Html,
    Json,
    Errors,
    Interrupt,
    ImmediateReuse,
    WireHtml,
    WireJson,
    WireErrors,
    WireImmediateReuse,
    WireBusy,
}
async fn interpreter_phase(phase: InterpreterPhase) -> Result {
    let sandbox = create(&manager()?, options()?).await?;
    let outcome = async {
        let interpreter = CodeInterpreter::create(sandbox.clone(), CodeInterpreterOptions::default()).await?;
        let codes = interpreter.codes();
        let context = codes.create_context("python").await?;
        let id = context.id.clone().ok_or("missing context ID")?;
        let options = RunCodeOptions { context: Some(context), language: None };
        let outcome = async {
            match phase {
                InterpreterPhase::State => {
                    let initialized = recorded_run(codes, "answer = 41", options.clone()).await?;
                    require(initialized.error.is_none() && initialized.complete, "context initialization failed")?;
                    let printed = recorded_run(codes, "print('stdout-only', flush=True)", options.clone()).await?;
                    require(printed.error.is_none() && printed.complete && printed.logs.stdout.iter().any(|line| line.text.contains("stdout-only")), "stdout-only execution lost")?;
                    let execution = recorded_run(codes, "answer + 1", options).await?;
                    require(execution.result.iter().any(|r| r.text.as_deref() == Some("42")), "context state lost")?;
                    require(codes.get_context(&id).await?.id.as_deref() == Some(&id), "context get mismatch")?;
                    require(codes.list_contexts(Some("python")).await?.iter().any(|c| c.id.as_deref() == Some(&id)), "context absent")?;
                }
                InterpreterPhase::Html | InterpreterPhase::Json => {
                    let html = matches!(phase, InterpreterPhase::Html);
                    let code = if html { "from IPython.display import display, HTML\ndisplay(HTML('<b>rust</b>'))\nprint('after-display', flush=True)\n42" } else { "from IPython.display import display, JSON\ndisplay(JSON({'answer': 42}))\nprint('after-display', flush=True)\n42" };
                    let rich = recorded_run(codes, code, options).await?;
                    require(rich.result.iter().any(|r| r.raw.contains_key(if html { "text/html" } else { "application/json" })), if html { "HTML result lost" } else { "JSON result lost" })?;
                    require(rich.complete && rich.error.is_none() && rich.result.iter().any(|r| r.text.as_deref() == Some("42")) && rich.logs.stdout.iter().any(|line| line.text.contains("after-display")), "trailing rich output/result lost")?;
                }
                InterpreterPhase::Errors => {
                    let failure = recorded_run(codes, "print('before-error', flush=True)\nraise ValueError('rust-e2e-error')", options).await?;
                    require(failure.error.is_some() && failure.logs.stdout.iter().any(|line| line.text.contains("before-error")), "kernel error/stdout lost")?;
                }
                InterpreterPhase::Interrupt => {
                    let interrupted = interrupted_execution(codes, &id, options.clone(), true).await?;
                    require(interrupted.error.is_some(), "interrupt did not yield kernel error")?;
                    let reused = recorded_run(codes, "answer + 1", options).await?;
                    require(reused.complete && reused.error.is_none() && reused.result.iter().any(|r| r.text.as_deref() == Some("42")), "context unusable after interrupt")?;
                }
                InterpreterPhase::ImmediateReuse => {
                    for _ in 0..20 {
                        let failure = recorded_run(codes, "answer = 41\nprint('stress-before-error', flush=True)\nraise ValueError('stress-error')", options.clone()).await?;
                        require(failure.error.is_some() && failure.logs.stdout.iter().any(|line| line.text.contains("stress-before-error")), "repeated error execution lost stdout/error")?;
                        let next = recorded_run(codes, "answer + 1", options.clone()).await?;
                        require(next.complete && next.error.is_none() && next.result.iter().any(|r| r.text.as_deref() == Some("42")), "repeated post-error execution lost state/result")?;
                    }
                }
                InterpreterPhase::WireBusy => concurrent_busy(&sandbox, codes, &id, options).await?,
                InterpreterPhase::WireHtml | InterpreterPhase::WireJson => {
                    let html = matches!(phase, InterpreterPhase::WireHtml);
                    let code = if html { "from IPython.display import display, HTML\ndisplay(HTML('<b>wire</b>'))\nprint('wire-before-complete', flush=True)\n42" } else { "from IPython.display import display, JSON\ndisplay(JSON({'answer': 42}))\nprint('wire-before-complete', flush=True)\n42" };
                    let events = interpreter_wire_events(&sandbox, &id, code, if html { "wire-html" } else { "wire-json" }).await?;
                    require(events.last().is_some_and(|e| e.event_type == "execution_complete") && events.iter().any(|e| e.results.as_ref().is_some_and(|r| if html { r.contains_key("text/html") } else { r.get("application/json").is_some_and(|value| value["answer"] == 42) })) && events.iter().any(|e| e.text.as_deref().is_some_and(|text| text.contains("wire-before-complete"))), "raw SSE lost rich output before completion")?;
                }
                InterpreterPhase::WireErrors | InterpreterPhase::WireImmediateReuse => {
                    let rounds = if matches!(phase, InterpreterPhase::WireImmediateReuse) { 20 } else { 1 };
                    for _ in 0..rounds {
                        let events = interpreter_wire_events(&sandbox, &id, "answer = 41\nprint('wire-before-error', flush=True)\nraise ValueError('wire-error')", "wire-error").await?;
                        require(events.last().is_some_and(|e| e.event_type == "error") && events.iter().any(|e| e.text.as_deref().is_some_and(|text| text.contains("wire-before-error"))), "raw SSE lost stdout before error")?;
                        if rounds > 1 {
                            let events = interpreter_wire_events(&sandbox, &id, "answer + 1", "wire-immediate-reuse").await?;
                            require(events.last().is_some_and(|e| e.event_type == "execution_complete") && events.iter().any(scalar_42), "raw immediate context reuse lost state/result")?;
                        }
                    }
                }
            }
            Ok(())
        }.await;
        finish_context(codes, &id, outcome).await
    }.await;
    finish(&sandbox, outcome).await
}

macro_rules! interpreter_tests {
    ($($name:ident => $phase:ident),+ $(,)?) => { $(
        #[tokio::test]
        #[ignore = "real code-interpreter image and Jupyter required"]
        async fn $name() -> Result { interpreter_phase(InterpreterPhase::$phase).await }
    )+ };
}
interpreter_tests! {
    docker_interpreter_state => State,
    docker_interpreter_html => Html,
    docker_interpreter_json => Json,
    docker_interpreter_errors => Errors,
    docker_interpreter_interruption => Interrupt,
    docker_interpreter_immediate_reuse => ImmediateReuse,
    docker_interpreter_wire_html => WireHtml,
    docker_interpreter_wire_json => WireJson,
    docker_interpreter_wire_errors => WireErrors,
    docker_interpreter_wire_immediate_reuse => WireImmediateReuse,
    docker_interpreter_wire_busy => WireBusy,
}
