    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut state = self.state.lock().expect("conversion watch state lock poisoned");
        loop {
            if state.pending.is_some() {
                let poll = state.pending.as_mut().expect("pending conversion future exists").as_mut().poll(cx);
                match poll {
                    Poll::Ready(result) => {
                        state.pending = None;
                        if let Some(result) = result {
                            return Poll::Ready(Some(result));
                        }
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            let event = match state.events.as_mut().poll_next(cx) {
                Poll::Ready(Some(event)) => event,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            };
            let kind = state.kind.clone();
            let api_version = state.api_version.clone();
            let storage = state.storage.clone();
            let group = state.group.clone();
            let resource = state.resource.clone();
            let version = state.version.clone();
            let partial_metadata = state.partial_metadata;
            let conversion_webhook = state.conversion_webhook.clone();
            state.pending = Some(Box::pin(async move {
                encode_watch_event_with_conversion(
                    &event,
                    &kind,
                    &api_version,
                    storage,
                    &group,
                    &resource,
                    &version,
                    partial_metadata,
                    conversion_webhook,
                )
                .await
            }));
        }
    }
