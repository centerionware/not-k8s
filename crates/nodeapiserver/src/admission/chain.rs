//! Ordered admission-plugin plumbing for pure mutators.
//!
//! Real upstream exposes admission through a plugin interface and runs the
//! registered plugins in a deterministic order. The listener still has
//! storage-backed admission steps that need their own I/O and error handling,
//! but pure mutators do not need to be hand-called there. This small registry
//! keeps those plugins interchangeable and makes their order explicit without
//! hiding storage access inside a generic callback.

use super::attributes::Operation;
use serde_json::Value;
use std::collections::BTreeSet;

/// The request facts a pure mutating plugin may use to decide whether it
/// applies. The object is borrowed mutably only while the chain runs.
pub struct Request<'a> {
    pub operation: Operation,
    pub group: &'a str,
    pub resource: &'a str,
    pub subresource: &'a str,
    pub namespace: &'a str,
    pub name: &'a str,
    pub old_object: Option<&'a Value>,
    pub object: &'a mut Value,
}

/// A pure mutating admission plugin. Storage-backed plugins remain separate
/// because their I/O and failure-policy behavior cannot be represented by a
/// synchronous `Value -> Value` callback without losing request semantics.
pub trait MutatingPlugin: Send + Sync {
    fn name(&self) -> &'static str;
    fn applies(&self, request: &Request<'_>) -> bool;
    fn mutate(&self, object: &mut Value);
}

/// An ordered registry of pure mutating admission plugins.
pub struct MutatingRegistry {
    plugins: Vec<Box<dyn MutatingPlugin>>,
}

impl MutatingRegistry {
    pub fn new() -> Self {
        Self { plugins: Vec::new() }
    }

    pub fn register<P>(&mut self, plugin: P)
    where
        P: MutatingPlugin + 'static,
    {
        self.plugins.push(Box::new(plugin));
    }

    /// Registers the default pure built-ins in their upstream order.
    pub fn with_builtins() -> Self {
        Self::with_builtins_enabled(&[])
    }

    /// Registers the default pure built-ins plus explicitly enabled
    /// upstream opt-in plugins.
    pub fn with_builtins_enabled(enabled: &[String]) -> Self {
        let mut registry = Self::new();
        registry.register(DefaultTolerationSeconds);
        registry.register(ExtendedResourceToleration);
        registry.register(ServiceAccountDefaults);
        if enabled.iter().any(|plugin| plugin.eq_ignore_ascii_case("AlwaysPullImages")) {
            registry.register(AlwaysPullImages);
        }
        registry.register(TaintNodesByCondition);
        registry
    }

    pub fn run(&self, request: &mut Request<'_>) {
        for plugin in &self.plugins {
            if plugin.applies(request) {
                plugin.mutate(request.object);
            }
        }
    }

    #[cfg(test)]
    fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.plugins.iter().map(|plugin| plugin.name())
    }
}

impl Default for MutatingRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

struct DefaultTolerationSeconds;

impl MutatingPlugin for DefaultTolerationSeconds {
    fn name(&self) -> &'static str {
        "DefaultTolerationSeconds"
    }

    fn applies(&self, request: &Request<'_>) -> bool {
        super::default_toleration_seconds::applies_to(
            request.group,
            request.resource,
            request.subresource,
        )
    }

    fn mutate(&self, object: &mut Value) {
        super::default_toleration_seconds::mutate(object);
    }
}

struct ServiceAccountDefaults;

struct ExtendedResourceToleration;

struct AlwaysPullImages;

impl MutatingPlugin for AlwaysPullImages {
    fn name(&self) -> &'static str {
        "AlwaysPullImages"
    }

    fn applies(&self, request: &Request<'_>) -> bool {
        if !request.group.is_empty() || request.resource != "pods" || !request.subresource.is_empty() {
            return false;
        }
        match request.operation {
            Operation::Create => true,
            Operation::Update => request.old_object.is_none_or(|old| has_new_image(old, request.object)),
            _ => false,
        }
    }

    fn mutate(&self, object: &mut Value) {
        let Some(spec) = object.get_mut("spec").and_then(Value::as_object_mut) else {
            return;
        };
        for field in ["initContainers", "containers", "ephemeralContainers"] {
            if let Some(containers) = spec.get_mut(field).and_then(Value::as_array_mut) {
                for container in containers {
                    if let Some(container) = container.as_object_mut() {
                        container.insert(
                            "imagePullPolicy".to_string(),
                            Value::String("Always".to_string()),
                        );
                    }
                }
            }
        }
        if let Some(volumes) = spec.get_mut("volumes").and_then(Value::as_array_mut) {
            for volume in volumes {
                if let Some(image) = volume.get_mut("image").and_then(Value::as_object_mut) {
                    image.insert(
                        "pullPolicy".to_string(),
                        Value::String("Always".to_string()),
                    );
                }
            }
        }
    }
}

fn has_new_image(old: &Value, new: &Value) -> bool {
    let old_images = container_images(old).collect::<BTreeSet<_>>();
    container_images(new).any(|image| !old_images.contains(image))
}

fn container_images(value: &Value) -> impl Iterator<Item = &str> {
    ["initContainers", "containers", "ephemeralContainers"]
        .into_iter()
        .flat_map(move |field| {
            value
                .pointer(&format!("/spec/{field}"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|container| container.get("image").and_then(Value::as_str))
        })
}

impl MutatingPlugin for ExtendedResourceToleration {
    fn name(&self) -> &'static str {
        "ExtendedResourceToleration"
    }

    fn applies(&self, request: &Request<'_>) -> bool {
        super::extended_resource_toleration::applies_to(
            request.operation,
            request.group,
            request.resource,
            request.subresource,
        )
    }

    fn mutate(&self, object: &mut Value) {
        super::extended_resource_toleration::mutate(object);
    }
}

impl MutatingPlugin for ServiceAccountDefaults {
    fn name(&self) -> &'static str {
        "ServiceAccount"
    }

    fn applies(&self, request: &Request<'_>) -> bool {
        request.operation == Operation::Create
            && super::service_account::applies_to(
                request.group,
                request.resource,
                request.subresource,
            )
    }

    fn mutate(&self, object: &mut Value) {
        super::service_account::default_service_account_name(object);
    }
}

struct TaintNodesByCondition;

impl MutatingPlugin for TaintNodesByCondition {
    fn name(&self) -> &'static str {
        "TaintNodesByCondition"
    }

    fn applies(&self, request: &Request<'_>) -> bool {
        super::taint_nodes_by_condition::applies_to(
            request.operation,
            request.group,
            request.resource,
            request.subresource,
        )
    }

    fn mutate(&self, object: &mut Value) {
        super::taint_nodes_by_condition::mutate(object);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Marker(&'static str);

    impl MutatingPlugin for Marker {
        fn name(&self) -> &'static str {
            self.0
        }

        fn applies(&self, _request: &Request<'_>) -> bool {
            true
        }

        fn mutate(&self, object: &mut Value) {
            object["order"] = json!(format!("{}{}", object["order"].as_str().unwrap_or(""), self.0));
        }
    }

    fn request<'a>(object: &'a mut Value, operation: Operation) -> Request<'a> {
        Request {
            operation,
            group: "",
            resource: "pods",
            subresource: "",
            namespace: "default",
            name: "pod",
            old_object: None,
            object,
        }
    }

    #[test]
    fn registry_runs_plugins_in_registration_order() {
        let mut registry = MutatingRegistry::new();
        registry.register(Marker("first"));
        registry.register(Marker("second"));
        assert_eq!(registry.names().collect::<Vec<_>>(), ["first", "second"]);

        let mut object = json!({"order": ""});
        registry.run(&mut request(&mut object, Operation::Create));
        assert_eq!(object["order"], "firstsecond");
    }

    #[test]
    fn builtins_preserve_the_existing_mutation_order_and_scope() {
        let registry = MutatingRegistry::with_builtins();
        assert_eq!(
            registry.names().collect::<Vec<_>>(),
            [
                "DefaultTolerationSeconds",
                "ExtendedResourceToleration",
                "ServiceAccount",
                "TaintNodesByCondition"
            ]
        );

        let mut pod = json!({"spec": {}});
        registry.run(&mut request(&mut pod, Operation::Create));
        assert_eq!(pod["spec"]["serviceAccountName"], "default");
        assert_eq!(pod["spec"]["tolerations"].as_array().unwrap().len(), 2);

        let mut status = json!({"spec": {}});
        let mut status_request = request(&mut status, Operation::Update);
        status_request.subresource = "status";
        registry.run(&mut status_request);
        assert!(status["spec"].get("serviceAccountName").is_none());
        assert!(status["spec"].get("tolerations").is_none());
    }

    #[test]
    fn always_pull_images_is_opt_in_and_updates_all_pod_container_lists() {
        let enabled = vec!["alwayspullimages".to_string()];
        let registry = MutatingRegistry::with_builtins_enabled(&enabled);
        assert!(registry.names().any(|name| name == "AlwaysPullImages"));

        let mut pod = json!({
            "spec": {
                "initContainers": [{"image": "init", "imagePullPolicy": "IfNotPresent"}],
                "containers": [{"image": "main", "imagePullPolicy": "IfNotPresent"}],
                "ephemeralContainers": [{"image": "debug", "imagePullPolicy": "IfNotPresent"}]
            }
        });
        registry.run(&mut request(&mut pod, Operation::Create));
        for field in ["initContainers", "containers", "ephemeralContainers"] {
            assert_eq!(pod["spec"][field][0]["imagePullPolicy"], "Always");
        }

        let old = json!({"spec": {"containers": [{"image": "main"}]}});
        let mut unchanged_image = json!({"spec": {"containers": [{"image": "main", "imagePullPolicy": "Never"}]} });
        let mut update_request = request(&mut unchanged_image, Operation::Update);
        update_request.old_object = Some(&old);
        registry.run(&mut update_request);
        assert_eq!(unchanged_image["spec"]["containers"][0]["imagePullPolicy"], "Never");

        let mut new_image = json!({"spec": {"containers": [{"image": "new", "imagePullPolicy": "Never"}]} });
        let mut update_request = request(&mut new_image, Operation::Update);
        update_request.old_object = Some(&old);
        registry.run(&mut update_request);
        assert_eq!(new_image["spec"]["containers"][0]["imagePullPolicy"], "Always");

        let mut default_pod = json!({"spec": {"containers": [{"image": "main"}]} });
        MutatingRegistry::with_builtins().run(&mut request(&mut default_pod, Operation::Create));
        assert!(default_pod["spec"]["containers"][0].get("imagePullPolicy").is_none());
    }
}
