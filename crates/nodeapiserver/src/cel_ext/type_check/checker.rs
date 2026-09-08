struct Checker {
    variables: HashMap<String, CelType>,
    errors: Vec<TypeError>,
    references_old_self: bool,
}

impl Checker {
    fn expression(&mut self, expression: &IdedExpr) -> CelType {
        match &expression.expr {
            Expr::Unspecified => CelType::Dyn,
            Expr::Literal(literal) => literal_type(literal),
            Expr::Ident(name) => self.identifier(name),
            Expr::Select(select) => {
                let operand = self.expression(&select.operand);
                if select.test {
                    self.selected_type(operand, &select.field);
                    CelType::Bool
                } else {
                    self.selected_type(operand, &select.field)
                }
            }
            Expr::Call(call) => {
                let target = call.target.as_ref().map(|target| self.expression(target));
                let arguments = call
                    .args
                    .iter()
                    .map(|argument| self.expression(argument))
                    .collect::<Vec<_>>();
                self.call(&call.func_name, target, &arguments)
            }
            Expr::List(list) => {
                let mut element = CelType::Dyn;
                for item in &list.elements {
                    element = unify(element, self.expression(item));
                }
                CelType::List(Box::new(element))
            }
            Expr::Map(map) => {
                let mut value = CelType::Dyn;
                for entry in &map.entries {
                    match &entry.expr {
                        EntryExpr::MapEntry(entry) => {
                            self.expression(&entry.key);
                            value = unify(value, self.expression(&entry.value));
                        }
                        EntryExpr::StructField(field) => {
                            value = unify(value, self.expression(&field.value))
                        }
                    }
                }
                CelType::Map(Box::new(value))
            }
            Expr::Struct(structure) => {
                for entry in &structure.entries {
                    match &entry.expr {
                        EntryExpr::MapEntry(entry) => {
                            self.expression(&entry.key);
                            self.expression(&entry.value);
                        }
                        EntryExpr::StructField(field) => {
                            self.expression(&field.value);
                        }
                    }
                }
                CelType::Dyn
            }
            Expr::Comprehension(comprehension) => self.comprehension(comprehension),
        }
    }

    fn identifier(&mut self, name: &str) -> CelType {
        if name == "oldSelf" {
            self.references_old_self = true;
        }
        if let Some(ty) = self.variables.get(name) {
            return ty.clone();
        }
        // Qualified extension functions (`format.date()` and
        // `ip.isCanonical()`) use a namespace identifier that the runtime
        // resolves without looking up as a variable.
        if matches!(name, "format" | "ip") || name.starts_with('@') {
            return CelType::Dyn;
        }
        self.errors
            .push(TypeError::UnknownIdentifier(name.to_string()));
        CelType::Dyn
    }

    fn selected_type(&mut self, operand: CelType, field: &str) -> CelType {
        match operand {
            CelType::Object { fields, additional } => fields
                .get(field)
                .cloned()
                .or_else(|| additional.map(|value| *value))
                .unwrap_or_else(|| {
                    self.errors.push(TypeError::UnknownField {
                        field: field.to_string(),
                        on: CelType::Object {
                            fields,
                            additional: None,
                        },
                    });
                    CelType::Dyn
                }),
            CelType::Map(value) => *value,
            CelType::Dyn => CelType::Dyn,
            other => {
                self.errors.push(TypeError::InvalidOperand {
                    operation: "field selection".to_string(),
                    expected: "object or map".to_string(),
                    actual: other,
                });
                CelType::Dyn
            }
        }
    }

    fn call(&mut self, name: &str, target: Option<CelType>, arguments: &[CelType]) -> CelType {
        match name {
            operators::CONDITIONAL => {
                if let Some(condition) = arguments.first() {
                    self.require_bool(name, condition);
                }
                match (arguments.get(1), arguments.get(2)) {
                    (Some(left), Some(right)) => unify(left.clone(), right.clone()),
                    _ => CelType::Dyn,
                }
            }
            operators::LOGICAL_AND | operators::LOGICAL_OR => {
                for argument in arguments {
                    self.require_bool(name, argument);
                }
                CelType::Bool
            }
            operators::LOGICAL_NOT | operators::NOT_STRICTLY_FALSE => {
                if let Some(argument) = arguments.first() {
                    self.require_bool(name, argument);
                }
                CelType::Bool
            }
            operators::NEGATE => {
                if let Some(argument) = arguments.first() {
                    self.require_numeric(name, argument);
                    argument.clone()
                } else {
                    CelType::Dyn
                }
            }
            operators::ADD => self.binary_add(arguments),
            operators::SUBSTRACT | operators::MULTIPLY | operators::DIVIDE | operators::MODULO => {
                self.binary_numeric(name, arguments)
            }
            operators::GREATER
            | operators::GREATER_EQUALS
            | operators::LESS
            | operators::LESS_EQUALS => {
                self.binary_comparable(name, arguments);
                CelType::Bool
            }
            operators::EQUALS | operators::NOT_EQUALS => {
                if let (Some(left), Some(right)) = (arguments.first(), arguments.get(1)) {
                    if !compatible(left, right) {
                        self.errors.push(TypeError::IncompatibleOperands {
                            operation: name.to_string(),
                            left: left.clone(),
                            right: right.clone(),
                        });
                    }
                }
                CelType::Bool
            }
            operators::IN => {
                if let Some(container) = arguments.get(1) {
                    if !matches!(
                        container,
                        CelType::Dyn | CelType::List(_) | CelType::Map(_) | CelType::String
                    ) {
                        self.errors.push(TypeError::InvalidOperand {
                            operation: name.to_string(),
                            expected: "list, map, or string".to_string(),
                            actual: container.clone(),
                        });
                    }
                }
                CelType::Bool
            }
            operators::INDEX | operators::OPT_INDEX => self.index(arguments),
            "has" => CelType::Bool,
            "size" => {
                if let Some(value) = arguments.first().or(target.as_ref()) {
                    if !matches!(
                        value,
                        CelType::Dyn
                            | CelType::String
                            | CelType::Bytes
                            | CelType::List(_)
                            | CelType::Map(_)
                    ) {
                        self.errors.push(TypeError::InvalidOperand {
                            operation: "size".to_string(),
                            expected: "string, bytes, list, or map".to_string(),
                            actual: value.clone(),
                        });
                    }
                }
                CelType::Int
            }
            "contains" | "startsWith" | "endsWith" | "matches" => {
                self.string_method(name, target.as_ref(), arguments);
                CelType::Bool
            }
            "find" => {
                self.string_method(name, target.as_ref(), arguments);
                CelType::String
            }
            "findAll" => {
                self.string_method(name, target.as_ref(), arguments);
                CelType::List(Box::new(CelType::String))
            }
            "isSorted" | "includes" => {
                self.list_method(name, target.as_ref(), arguments);
                CelType::Bool
            }
            "min" | "max" | "sum" => {
                self.list_method(name, target.as_ref(), arguments);
                target
                    .and_then(|target| match target {
                        CelType::List(element) => Some(*element),
                        _ => None,
                    })
                    .unwrap_or(CelType::Dyn)
            }
            "indexOf" | "lastIndexOf" => {
                self.list_method(name, target.as_ref(), arguments);
                CelType::Int
            }
            "isQuantity" | "isIP" | "isCIDR" | "isURL" => {
                self.require_argument(name, arguments, 0, "string", is_string_type);
                CelType::Bool
            }
            "isSemver" => {
                self.require_argument(name, arguments, 0, "string", is_string_type);
                self.require_argument(name, arguments, 1, "bool", is_bool_type);
                CelType::Bool
            }
            "ip.isCanonical" => {
                self.require_argument(name, arguments, 0, "string", is_string_type);
                CelType::Bool
            }
            "quantity" => {
                if self.require_argument(name, arguments, 0, "string", is_string_type) {
                    CelType::Quantity
                } else {
                    CelType::Dyn
                }
            }
            "ip" => {
                if target.is_some() {
                    if self.require_receiver(name, target.as_ref(), "a CIDR", is_cidr_type) {
                        CelType::Ip
                    } else {
                        CelType::Dyn
                    }
                } else if self.require_argument(name, arguments, 0, "string", is_string_type) {
                    CelType::Ip
                } else {
                    CelType::Dyn
                }
            }
            "cidr" => {
                if self.require_argument(name, arguments, 0, "string", is_string_type) {
                    CelType::Cidr
                } else {
                    CelType::Dyn
                }
            }
            "url" => {
                if self.require_argument(name, arguments, 0, "string", is_string_type) {
                    CelType::Url
                } else {
                    CelType::Dyn
                }
            }
            "semver" => {
                let valid = self.require_argument(name, arguments, 0, "string", is_string_type);
                self.require_argument(name, arguments, 1, "bool", is_bool_type);
                if valid { CelType::Semver } else { CelType::Dyn }
            }
            "isInteger" => {
                self.require_receiver(name, target.as_ref(), "a Quantity", is_quantity_type);
                CelType::Bool
            }
            "asInteger" => {
                if self.require_receiver(name, target.as_ref(), "a Quantity", is_quantity_type) {
                    CelType::Int
                } else {
                    CelType::Dyn
                }
            }
            "asApproximateFloat" => {
                if self.require_receiver(name, target.as_ref(), "a Quantity", is_quantity_type) {
                    CelType::Double
                } else {
                    CelType::Dyn
                }
            }
            "sign" => {
                if self.require_receiver(name, target.as_ref(), "a Quantity", is_quantity_type) {
                    CelType::Int
                } else {
                    CelType::Dyn
                }
            }
            "add" | "sub" => {
                if self.require_receiver(name, target.as_ref(), "a Quantity", is_quantity_type) {
                    self.require_argument(
                        name,
                        arguments,
                        0,
                        "a Quantity or integer",
                        is_quantity_or_int_type,
                    );
                    CelType::Quantity
                } else {
                    CelType::Dyn
                }
            }
            "isLessThan" | "isGreaterThan" | "compareTo" => {
                self.comparison_method(name, target.as_ref(), arguments)
            }
            "family" => {
                if self.require_receiver(name, target.as_ref(), "an IP", is_ip_type) {
                    CelType::Int
                } else {
                    CelType::Dyn
                }
            }
            "isUnspecified"
            | "isLoopback"
            | "isLinkLocalMulticast"
            | "isLinkLocalUnicast"
            | "isGlobalUnicast" => {
                self.require_receiver(name, target.as_ref(), "an IP", is_ip_type);
                CelType::Bool
            }
            "prefixLength" => {
                if self.require_receiver(name, target.as_ref(), "a CIDR", is_cidr_type) {
                    CelType::Int
                } else {
                    CelType::Dyn
                }
            }
            "containsIP" => {
                if self.require_receiver(name, target.as_ref(), "a CIDR", is_cidr_type) {
                    self.require_argument(
                        name,
                        arguments,
                        0,
                        "an IP or string",
                        is_ip_or_string_type,
                    );
                    CelType::Bool
                } else {
                    CelType::Dyn
                }
            }
            "containsCIDR" => {
                if self.require_receiver(name, target.as_ref(), "a CIDR", is_cidr_type) {
                    self.require_argument(
                        name,
                        arguments,
                        0,
                        "a CIDR or string",
                        is_cidr_or_string_type,
                    );
                    CelType::Bool
                } else {
                    CelType::Dyn
                }
            }
            "masked" => {
                if self.require_receiver(name, target.as_ref(), "a CIDR", is_cidr_type) {
                    CelType::Cidr
                } else {
                    CelType::Dyn
                }
            }
            "getScheme" | "getHost" | "getHostname" | "getPort" | "getEscapedPath" => {
                if self.require_receiver(name, target.as_ref(), "a URL", is_url_type) {
                    CelType::String
                } else {
                    CelType::Dyn
                }
            }
            "getQuery" => {
                if self.require_receiver(name, target.as_ref(), "a URL", is_url_type) {
                    CelType::Map(Box::new(CelType::List(Box::new(CelType::String))))
                } else {
                    CelType::Dyn
                }
            }
            "major" | "minor" | "patch" => {
                if self.require_receiver(name, target.as_ref(), "a Semver", is_semver_type) {
                    CelType::Int
                } else {
                    CelType::Dyn
                }
            }
            "format.dns1123Label"
            | "format.dns1123Subdomain"
            | "format.dns1035Label"
            | "format.qualifiedName"
            | "format.dns1123LabelPrefix"
            | "format.dns1123SubdomainPrefix"
            | "format.dns1035LabelPrefix"
            | "format.labelValue"
            | "format.uri"
            | "format.uuid"
            | "format.byte"
            | "format.date"
            | "format.datetime" => CelType::Format,
            "format.named" => {
                self.require_argument(name, arguments, 0, "string", is_string_type);
                CelType::Optional(Box::new(CelType::Format))
            }
            "validate" => {
                if self.require_receiver(name, target.as_ref(), "a named format", is_format_type) {
                    self.require_argument(name, arguments, 0, "string", is_string_type);
                    CelType::Optional(Box::new(CelType::List(Box::new(CelType::String))))
                } else {
                    CelType::Dyn
                }
            }
            "hasValue" => {
                self.require_receiver(name, target.as_ref(), "an optional value", is_optional_type);
                CelType::Bool
            }
            "value" => match target.as_ref() {
                Some(CelType::Optional(value)) => (**value).clone(),
                Some(CelType::Dyn) | None => CelType::Dyn,
                Some(actual) => {
                    self.errors.push(TypeError::InvalidOperand {
                        operation: name.to_string(),
                        expected: "an optional value receiver".to_string(),
                        actual: actual.clone(),
                    });
                    CelType::Dyn
                }
            },
            "string" => {
                let value = target.as_ref().or_else(|| arguments.first());
                if let Some(value) = value {
                    if !matches!(
                        value,
                        CelType::Dyn
                            | CelType::String
                            | CelType::Int
                            | CelType::Uint
                            | CelType::Double
                            | CelType::Bytes
                            | CelType::Ip
                            | CelType::Cidr
                            | CelType::Url
                            | CelType::Semver
                    ) {
                        self.errors.push(TypeError::InvalidOperand {
                            operation: name.to_string(),
                            expected: "a string-convertible value".to_string(),
                            actual: value.clone(),
                        });
                    }
                }
                CelType::String
            }
            "fieldSelector" | "labelSelector" | "group" | "resource" | "subresource"
            | "namespace" | "name" | "serviceAccount" | "check" | "allowed" | "errored"
            | "error" | "reason" => CelType::Dyn,
            _ if name.starts_with("format.") || name.starts_with("ip.") => CelType::Dyn,
            _ => CelType::Dyn,
        }
    }

    fn require_receiver(
        &mut self,
        operation: &str,
        target: Option<&CelType>,
        expected: &str,
        predicate: fn(&CelType) -> bool,
    ) -> bool {
        match target {
            Some(CelType::Dyn) => true,
            Some(actual) if predicate(actual) => true,
            Some(actual) => {
                self.errors.push(TypeError::InvalidOperand {
                    operation: operation.to_string(),
                    expected: format!("{expected} receiver"),
                    actual: actual.clone(),
                });
                false
            }
            None => false,
        }
    }

    fn require_argument(
        &mut self,
        operation: &str,
        arguments: &[CelType],
        index: usize,
        expected: &str,
        predicate: fn(&CelType) -> bool,
    ) -> bool {
        match arguments.get(index) {
            Some(CelType::Dyn) => true,
            Some(actual) if predicate(actual) => true,
            Some(actual) => {
                self.errors.push(TypeError::InvalidOperand {
                    operation: operation.to_string(),
                    expected: expected.to_string(),
                    actual: actual.clone(),
                });
                false
            }
            None => false,
        }
    }

    fn comparison_method(
        &mut self,
        name: &str,
        target: Option<&CelType>,
        arguments: &[CelType],
    ) -> CelType {
        let Some(target) = target else {
            return CelType::Dyn;
        };
        let valid = match target {
            CelType::Dyn => true,
            CelType::Quantity => {
                self.require_argument(name, arguments, 0, "a Quantity", is_quantity_type)
            }
            CelType::Semver => {
                self.require_argument(name, arguments, 0, "a Semver", is_semver_type)
            }
            actual => {
                self.errors.push(TypeError::InvalidOperand {
                    operation: name.to_string(),
                    expected: "a Quantity or Semver receiver".to_string(),
                    actual: actual.clone(),
                });
                false
            }
        };
        if valid {
            if name == "compareTo" {
                CelType::Int
            } else {
                CelType::Bool
            }
        } else {
            CelType::Dyn
        }
    }

    fn binary_add(&mut self, arguments: &[CelType]) -> CelType {
        let (Some(left), Some(right)) = (arguments.first(), arguments.get(1)) else {
            return CelType::Dyn;
        };
        if left.is_dynamic() || right.is_dynamic() {
            return CelType::Dyn;
        }
        if matches!((left, right), (CelType::String, CelType::String)) {
            return CelType::String;
        }
        if let (CelType::List(left), CelType::List(right)) = (left, right) {
            return CelType::List(Box::new(unify((**left).clone(), (**right).clone())));
        }
        if left.is_numeric() && right.is_numeric() {
            return unify(left.clone(), right.clone());
        }
        self.errors.push(TypeError::IncompatibleOperands {
            operation: operators::ADD.to_string(),
            left: left.clone(),
            right: right.clone(),
        });
        CelType::Dyn
    }

    fn binary_numeric(&mut self, name: &str, arguments: &[CelType]) -> CelType {
        let (Some(left), Some(right)) = (arguments.first(), arguments.get(1)) else {
            return CelType::Dyn;
        };
        if left.is_dynamic() || right.is_dynamic() {
            return CelType::Dyn;
        }
        if left.is_numeric() && right.is_numeric() {
            return unify(left.clone(), right.clone());
        }
        self.errors.push(TypeError::IncompatibleOperands {
            operation: name.to_string(),
            left: left.clone(),
            right: right.clone(),
        });
        CelType::Dyn
    }

    fn binary_comparable(&mut self, name: &str, arguments: &[CelType]) {
        let (Some(left), Some(right)) = (arguments.first(), arguments.get(1)) else {
            return;
        };
        if left.is_dynamic()
            || right.is_dynamic()
            || compatible(left, right) && (left.is_numeric() && right.is_numeric() || left == right)
        {
            return;
        }
        self.errors.push(TypeError::IncompatibleOperands {
            operation: name.to_string(),
            left: left.clone(),
            right: right.clone(),
        });
    }

    fn index(&mut self, arguments: &[CelType]) -> CelType {
        let (Some(container), Some(index)) = (arguments.first(), arguments.get(1)) else {
            return CelType::Dyn;
        };
        match container {
            CelType::Dyn => CelType::Dyn,
            CelType::List(element) => {
                if !matches!(index, CelType::Dyn | CelType::Int | CelType::Uint) {
                    self.errors.push(TypeError::InvalidOperand {
                        operation: "list index".to_string(),
                        expected: "int or uint".to_string(),
                        actual: index.clone(),
                    });
                }
                (**element).clone()
            }
            CelType::Map(value) => {
                if !matches!(index, CelType::Dyn | CelType::String) {
                    self.errors.push(TypeError::InvalidOperand {
                        operation: "map index".to_string(),
                        expected: "string".to_string(),
                        actual: index.clone(),
                    });
                }
                (**value).clone()
            }
            other => {
                self.errors.push(TypeError::InvalidOperand {
                    operation: "index".to_string(),
                    expected: "list or map".to_string(),
                    actual: other.clone(),
                });
                CelType::Dyn
            }
        }
    }

    fn string_method(&mut self, name: &str, target: Option<&CelType>, arguments: &[CelType]) {
        if let Some(target) = target {
            if !matches!(target, CelType::Dyn | CelType::String) {
                self.errors.push(TypeError::InvalidOperand {
                    operation: name.to_string(),
                    expected: "string receiver".to_string(),
                    actual: target.clone(),
                });
            }
        }
        if let Some(pattern) = arguments.first() {
            if !matches!(pattern, CelType::Dyn | CelType::String) {
                self.errors.push(TypeError::InvalidOperand {
                    operation: name.to_string(),
                    expected: "string argument".to_string(),
                    actual: pattern.clone(),
                });
            }
        }
        if name == "findAll" {
            if let Some(limit) = arguments.get(1) {
                if !matches!(limit, CelType::Dyn | CelType::Int) {
                    self.errors.push(TypeError::InvalidOperand {
                        operation: name.to_string(),
                        expected: "an optional int limit".to_string(),
                        actual: limit.clone(),
                    });
                }
            }
        }
    }

    fn list_method(&mut self, name: &str, target: Option<&CelType>, _arguments: &[CelType]) {
        if let Some(target) = target {
            if !matches!(target, CelType::Dyn | CelType::List(_)) {
                self.errors.push(TypeError::InvalidOperand {
                    operation: name.to_string(),
                    expected: "list receiver".to_string(),
                    actual: target.clone(),
                });
            }
        }
    }

    fn require_bool(&mut self, operation: &str, actual: &CelType) {
        if !actual.is_bool_or_dynamic() {
            self.errors.push(TypeError::InvalidOperand {
                operation: operation.to_string(),
                expected: "bool".to_string(),
                actual: actual.clone(),
            });
        }
    }

    fn require_numeric(&mut self, operation: &str, actual: &CelType) {
        if !actual.is_dynamic() && !actual.is_numeric() {
            self.errors.push(TypeError::InvalidOperand {
                operation: operation.to_string(),
                expected: "a number".to_string(),
                actual: actual.clone(),
            });
        }
    }

    fn comprehension(&mut self, comprehension: &cel::common::ast::ComprehensionExpr) -> CelType {
        let range = self.expression(&comprehension.iter_range);
        let element = match range {
            CelType::List(element) => *element,
            CelType::Map(value) => *value,
            CelType::Dyn => CelType::Dyn,
            other => {
                self.errors.push(TypeError::InvalidOperand {
                    operation: "comprehension".to_string(),
                    expected: "list or map range".to_string(),
                    actual: other,
                });
                CelType::Dyn
            }
        };
        let previous_iter = self
            .variables
            .insert(comprehension.iter_var.clone(), element.clone());
        let previous_iter2 = comprehension
            .iter_var2
            .as_ref()
            .map(|name| self.variables.insert(name.clone(), CelType::String));
        let accumulator = self.expression(&comprehension.accu_init);
        let previous_accumulator = self
            .variables
            .insert(comprehension.accu_var.clone(), accumulator);
        let condition = self.expression(&comprehension.loop_cond);
        self.require_bool("comprehension condition", &condition);
        self.expression(&comprehension.loop_step);
        let result = self.expression(&comprehension.result);
        restore(&mut self.variables, &comprehension.iter_var, previous_iter);
        if let Some(name) = &comprehension.iter_var2 {
            restore(
                &mut self.variables,
                name,
                previous_iter2.expect("iter_var2 was inserted"),
            );
        }
        restore(
            &mut self.variables,
            &comprehension.accu_var,
            previous_accumulator,
        );
        result
    }
}
