use super::{does_finalizer_exist, update_status_if_different, SyncHandler, UpdateError};
use crate::handler::{FinalizeResponse, Handler, SyncRequest};
use crate::resource::K8sResource;
use crate::runner::client::{Client, Patch};
use crate::runner::informer::{EventType, ResourceMessage};
use crate::runner::{duration_to_millis, RuntimeConfig};

use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) async fn handle_finalize(handler: SyncHandler) {
    let SyncHandler {
        mut sender,
        request,
        handler,
        client,
        runtime_config,
        parent_index_key,
    } = handler;

    let parent_id = request.parent.get_object_id().to_owned();
    let parent_id_ref = parent_id.as_id_ref();
    let parent_type = runtime_config.parent_type;

    let result = get_finalize_result(request, handler, client, &*runtime_config).await;
    let update_result = match result {
        Ok(retry) => {
            log::debug!(
                "Finalize handler for parent: {} completed without error",
                parent_id
            );
            Ok(retry)
        }
        Err(err) => {
            runtime_config.metrics.parent_sync_error(&parent_id_ref);
            log::error!("Failed to finalize parent: {}, err: {}", parent_id, err);
            Err(())
        }
    };
    let message = ResourceMessage {
        event_type: EventType::UpdateOperationComplete {
            result: update_result,
        },
        resource_type: parent_type,
        resource_id: parent_id,
        index_key: Some(parent_index_key),
    };
    let _ = sender.send(message).await;
}

async fn get_finalize_result(
    request: SyncRequest,
    handler: Arc<dyn Handler>,
    client: Client,
    runtime_config: &RuntimeConfig,
) -> Result<Option<Duration>, UpdateError> {
    if !does_finalizer_exist(&request.parent, runtime_config) {
        // we've already finalized this, so no need to do it again
        return Ok(None);
    }

    let (req, finalize_result) = tokio::task::spawn_blocking(move || {
        let start_time = Instant::now();
        let result = handler
            .finalize(&request)
            .map_err(UpdateError::HandlerError);
        {
            log::debug!(
                "finished invoking handler for parent: {} in {}ms",
                request.parent.get_object_id(),
                duration_to_millis(start_time.elapsed())
            );
        }
        (request, result)
    })
    .await?;
    let FinalizeResponse { retry, status } = finalize_result?;

    let request: SyncRequest = req;
    let parent_id = request.parent.get_object_id();

    if let Some(delay) = retry {
        log::info!(
            "handler response indicates that parent: {} has not been finalized. Will re-try later",
            parent_id
        );
        update_status_if_different(&request.parent, &client, runtime_config, status).await?;
        tokio::time::delay_for(delay).await;
    } else {
        log::info!(
            "handler response indicates that parent: {} has been finalized",
            parent_id
        );
        remove_finalizer(&client, runtime_config, &request.parent).await?;
    }

    Ok(retry)
}

async fn remove_finalizer<'a>(
    client: &Client,
    runtime_config: &RuntimeConfig,
    parent: &K8sResource,
) -> Result<(), UpdateError> {
    let id = parent.get_object_id();
    let k8s_type = &*runtime_config.parent_type;
    let patch = Patch::remove_finalizer(parent, runtime_config.operator_name.as_str());
    client.patch_resource(k8s_type, &id, &patch).await?;
    Ok(())
}
