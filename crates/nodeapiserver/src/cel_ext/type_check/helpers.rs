fn is_bool_type(value: &CelType) -> bool {
    matches!(value, CelType::Bool)
}

fn is_string_type(value: &CelType) -> bool {
    matches!(value, CelType::String)
}

fn is_quantity_type(value: &CelType) -> bool {
    matches!(value, CelType::Quantity)
}

fn is_ip_type(value: &CelType) -> bool {
    matches!(value, CelType::Ip)
}

fn is_cidr_type(value: &CelType) -> bool {
    matches!(value, CelType::Cidr)
}

fn is_url_type(value: &CelType) -> bool {
    matches!(value, CelType::Url)
}

fn is_semver_type(value: &CelType) -> bool {
    matches!(value, CelType::Semver)
}

fn is_format_type(value: &CelType) -> bool {
    matches!(value, CelType::Format)
}

fn is_optional_type(value: &CelType) -> bool {
    matches!(value, CelType::Optional(_))
}

fn is_quantity_or_int_type(value: &CelType) -> bool {
    matches!(value, CelType::Quantity | CelType::Int)
}

fn is_ip_or_string_type(value: &CelType) -> bool {
    matches!(value, CelType::Ip | CelType::String)
}

fn is_cidr_or_string_type(value: &CelType) -> bool {
    matches!(value, CelType::Cidr | CelType::String)
}

fn restore(variables: &mut HashMap<String, CelType>, name: &str, previous: Option<CelType>) {
    if let Some(previous) = previous {
        variables.insert(name.to_string(), previous);
    } else {
        variables.remove(name);
    }
}

fn literal_type(literal: &LiteralValue) -> CelType {
    match literal {
        LiteralValue::Boolean(_) => CelType::Bool,
        LiteralValue::Bytes(_) => CelType::Bytes,
        LiteralValue::Double(_) => CelType::Double,
        LiteralValue::Int(_) => CelType::Int,
        LiteralValue::Null => CelType::Null,
        LiteralValue::String(_) => CelType::String,
        LiteralValue::UInt(_) => CelType::Uint,
    }
}

fn compatible(left: &CelType, right: &CelType) -> bool {
    left.is_dynamic()
        || right.is_dynamic()
        || matches!(left, CelType::Null)
        || matches!(right, CelType::Null)
        || left == right
        || left.is_numeric() && right.is_numeric()
}

fn unify(left: CelType, right: CelType) -> CelType {
    if left.is_dynamic() {
        return right;
    }
    if right.is_dynamic() || left == right {
        return left;
    }
    if left.is_numeric() && right.is_numeric() {
        if matches!(left, CelType::Double) || matches!(right, CelType::Double) {
            CelType::Double
        } else if matches!(left, CelType::Uint) && matches!(right, CelType::Uint) {
            CelType::Uint
        } else {
            CelType::Int
        }
    } else {
        CelType::Dyn
    }
}

#[cfg(test)]
