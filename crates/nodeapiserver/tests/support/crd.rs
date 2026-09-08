//! CRD schema fixture, included only by CRD integration tests.

use serde_json::json;

pub fn a_crd() -> serde_json::Value {
    json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": {"name": "widgets.example.com"},
        "spec": {
            "group": "example.com",
            "scope": "Namespaced",
            "names": {"plural": "widgets", "singular": "widget", "kind": "Widget", "listKind": "WidgetList"},
            "versions": [{
                "name": "v1",
                "served": true,
                "storage": true,
                "additionalPrinterColumns": [{"name": "Color", "type": "string", "jsonPath": ".spec.color"}],
                "schema": {
                    "openAPIV3Schema": {
                        "type": "object",
                        "properties": {
                            "spec": {
                                "type": "object",
                                "properties": {
                                    "size": {"type": "string", "default": "small"},
                                    "color": {"type": "string"},
                                },
                            },
                        },
                    },
                },
            }],
        },
    })
}
