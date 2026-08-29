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

/// The request facts a pure mutating plugin may use to decide whether it
/// applies. The object is borrowed mutably only while the chain runs.
pub struct Request<'a> {
    pub operation: Operation,
    pub group: &'a str,
    pub resource: &'a str,
    pub subresource: &'a str,
    pub namespace: &'a str,
    pub name: &'a str,
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

    /// Registers the pure built-ins in the same order the listener used
    /// before this registry existed: default pod tolerations first, then
    /// ServiceAccount-name defaulting.
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(DefaultTolerationSeconds);
        registry.register(ServiceAccountDefaults);
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
        assert_eq!(registry.names().collect::<Vec<_>>(), ["DefaultTolerationSeconds", "ServiceAccount"]);

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
}
