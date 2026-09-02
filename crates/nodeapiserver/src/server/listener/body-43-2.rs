    aggregated.iter().find(|(g, v)| g == &parts[1] && v == &parts[2]).map(|(g, v)| (g.as_str(), v.as_str()))
