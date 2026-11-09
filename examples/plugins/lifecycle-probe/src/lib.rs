//! Integration probe for the WASM Kong lifecycle ABI.

#[allow(warnings)]
mod bindings;

use octra_plugin_sdk::{
    ApiPlugin, Decision, Error, Event, Job, ResponseBodyContext, ResponseContext,
    ResponseDecision,
};

struct LifecycleProbe;

impl ApiPlugin for LifecycleProbe {
    fn configure(_config: serde_json::Value) -> Result<Self, Error> {
        Ok(Self)
    }

    fn on_response(&self, _cx: &mut ResponseContext) -> Result<ResponseDecision, Error> {
        Ok(ResponseDecision::Continue)
    }

    fn on_header_filter(&self, cx: &mut ResponseContext) -> Result<ResponseDecision, Error> {
        cx.set_response_header("x-lifecycle-probe", "header-filter");
        Ok(ResponseDecision::Continue)
    }

    fn on_body_filter(
        &self,
        cx: &mut ResponseBodyContext,
        chunk: Option<Vec<u8>>,
        eof: bool,
    ) -> Result<Option<Vec<u8>>, Error> {
        if let Some(chunk) = chunk {
            cx.append_body(&chunk)?;
        }
        if !eof {
            return Ok(None);
        }
        let body = cx.take_body()?;
        let output = format!("{}:{}", cx.request_id(), String::from_utf8_lossy(&body));
        Ok(Some(output.into_bytes()))
    }

    fn on_event(&self, event: &Event) -> Result<(), Error> {
        if event.topic == "probe.fail" {
            return Err(Error::internal("probe event failure"));
        }
        Ok(())
    }

    fn on_job(&self, job: &Job) -> Result<(), Error> {
        if job.name == "probe-fail" {
            return Err(Error::internal("probe job failure"));
        }
        Ok(())
    }

    fn on_access(&self, _cx: &mut octra_plugin_sdk::RequestContext) -> Result<Decision, Error> {
        Ok(Decision::Continue)
    }
}

octra_plugin_sdk::export_plugin!(LifecycleProbe, bindings);
