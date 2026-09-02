    let Some(header) = accept_header else { return false };
    let Some(accepted) = negotiation::negotiate(header) else { return false };
