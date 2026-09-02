mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "replicas": {"type": "integer"},
                "tags": {"type": "array", "items": {"type": "object", "properties": {"name": {"type": "string"}}}},
            },
        })
    }

    #[test]
    fn declared_fields_and_nested_comprehension_variables_are_typed() {
        assert!(check_rule(
            &schema(),
            "self.replicas > 0 && self.tags.all(tag, tag.name != '')"
        )
        .is_empty());
    }

    #[test]
    fn an_undeclared_field_is_rejected() {
        let errors = check_rule(&schema(), "self.missing == 'x'");
        assert!(errors.iter().any(
            |error| matches!(error, TypeError::UnknownField { field, .. } if field == "missing")
        ));
    }

    #[test]
    fn an_obvious_operand_mismatch_is_rejected() {
        let errors = check_rule(&schema(), "self.name + 1");
        assert!(errors
            .iter()
            .any(|error| matches!(error, TypeError::IncompatibleOperands { .. })));
    }

    #[test]
    fn validation_rules_must_be_boolean() {
        let errors = check_rule(&schema(), "self.name");
        assert!(errors
            .iter()
            .any(|error| matches!(error, TypeError::NonBoolean(CelType::String))));
    }

    #[test]
    fn root_metadata_is_available_even_when_the_crd_schema_omits_it() {
        assert!(check_root_rule(
            &schema(),
            "self.metadata.name != '' && self.apiVersion != ''"
        )
        .is_empty());
    }

    #[test]
    fn dynamic_schema_sections_do_not_produce_false_positive_field_errors() {
        let schema = json!({"type": "object", "properties": {"data": {"type": "object", "x-kubernetes-preserve-unknown-fields": true}}});
        assert!(check_rule(&schema, "self.data.anything == 1").is_empty());
    }

    #[test]
    fn kubernetes_extension_values_and_format_optionals_are_typed() {
        let rule = "quantity(self.name).add(1).isGreaterThan(quantity('0')) && cidr('10.0.0.0/8').containsIP(ip('10.1.2.3')) && ip.isCanonical(self.name) && url(self.name).getQuery()['key'][0] == 'value' && semver('1.2.3').major() == 1 && format.named('uuid').value().validate(self.name).hasValue()";
        assert!(check_rule(&schema(), rule).is_empty());
    }

    #[test]
    fn kubernetes_extension_overloads_reject_wrong_operands() {
        let errors = check_rule(
            &schema(),
            "quantity(self.name).add('1') == quantity('2') || ip(self.name).family() == '4' || url(self.name).getHostname().family() == 4",
        );
        assert!(errors.iter().any(|error| matches!(
            error,
            TypeError::InvalidOperand { operation, .. } if operation == "add"
        )));
        assert!(errors.iter().any(|error| matches!(
            error,
            TypeError::IncompatibleOperands { .. }
        )));
        assert!(errors.iter().any(|error| matches!(
            error,
            TypeError::InvalidOperand { operation, .. } if operation == "family"
        )));
    }

    #[test]
    fn kubernetes_extension_constructors_require_strings() {
        let errors = check_rule(&schema(), "quantity(self.replicas) == quantity('1')");
        assert!(errors.iter().any(|error| matches!(
            error,
            TypeError::InvalidOperand { operation, .. } if operation == "quantity"
        )));
    }

    #[test]
    fn optional_old_self_exposes_has_value_and_value() {
        assert!(check_root_rule_with_optional_old_self(
            &schema(),
            "oldSelf.hasValue() ? oldSelf.value().name != '' : true",
            true,
        )
        .is_empty());
    }

    #[test]
    fn non_optional_old_self_rejects_optional_methods() {
        let errors = check_root_rule_with_optional_old_self(&schema(), "oldSelf.hasValue()", false);
        assert!(errors.iter().any(|error| matches!(
            error,
            TypeError::InvalidOperand { operation, .. } if operation == "hasValue"
        )));
    }

    #[test]
    fn kubernetes_escapes_reserved_and_punctuation_bearing_property_names() {
        assert_eq!(cel_field_name("namespace"), "__namespace__");
        assert_eq!(cel_field_name("x-y.z/a"), "x__dash__y__dot__z__slash__a");
        assert_eq!(cel_field_name("x__y"), "x__underscores__y");
    }

    #[test]
    fn escaped_schema_properties_are_available_to_the_type_checker() {
        let schema = json!({
            "type": "object",
            "properties": {"namespace": {"type": "string"}},
        });
        assert!(check_rule(&schema, "self.__namespace__ == 'default'").is_empty());
        assert!(!check_rule(&schema, "self.namespace == 'default'").is_empty());
    }
}
