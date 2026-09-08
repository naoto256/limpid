//! Preserve Windows metadata names and values without vendor normalization.
//! Raw XML retains UserData, namespaces and any fields not projected below.
pub fn encode(xml: &str) -> Result<Vec<u8>, String> {
    let document = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
    let event = document.root_element();
    if event.tag_name().name() != "Event" {
        return Err("expected Event XML root".into());
    }
    let mut object = serde_json::Map::new();
    object.insert("Xml".into(), xml.into());
    if let Some(system) = event.children().find(|n| n.has_tag_name("System")) {
        for field in system.children().filter(|n| n.is_element()) {
            let value = if field.attributes().len() != 0 {
                let mut attributes: serde_json::Map<String, serde_json::Value> = field
                    .attributes()
                    .map(|a| (a.name().to_owned(), a.value().into()))
                    .collect();
                if let Some(text) = field.text().filter(|text| !text.trim().is_empty()) {
                    attributes.insert("Value".into(), text.into());
                }
                serde_json::Value::Object(attributes)
            } else {
                field.text().unwrap_or("").into()
            };
            object.insert(field.tag_name().name().to_owned(), value);
        }
    }
    if let Some(data) = event.children().find(|n| n.has_tag_name("EventData")) {
        let values: Vec<_> = data.children().filter(|n| n.has_tag_name("Data")).map(|n| {
            serde_json::json!({"Name": n.attribute("Name"), "Value": n.text().unwrap_or("")})
        }).collect();
        object.insert("EventData".into(), values.into());
    }
    serde_json::to_vec(&object).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_metadata_raw_xml_and_duplicate_data_are_preserved() {
        let xml = r#"<Event xmlns="http://schemas.microsoft.com/win/2004/08/events/event"><System><Provider Name="Synthetic" Guid="{synthetic}"/><EventID>42</EventID><Channel>System</Channel><TimeCreated SystemTime="2026-01-01T00:00:00Z"/></System><EventData><Data Name="value">one &amp; two</Data><Data Name="value">三</Data></EventData><UserData><Unknown>kept</Unknown></UserData></Event>"#;
        let value: serde_json::Value =
            serde_json::from_slice(&super::encode(xml).unwrap()).unwrap();
        assert_eq!(value["Xml"], xml);
        assert_eq!(value["Provider"]["Name"], "Synthetic");
        assert_eq!(value["EventID"], "42");
        assert_eq!(value["EventData"][0]["Value"], "one & two");
        assert_eq!(value["EventData"][1]["Value"], "三");
    }
    #[test]
    fn system_text_is_not_lost_when_attributes_are_present() {
        let value: serde_json::Value = serde_json::from_slice(
            &super::encode(
                r#"<Event><System><EventID Qualifiers="0">42</EventID></System></Event>"#,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(value["EventID"]["Value"], "42");
        assert_eq!(value["EventID"]["Qualifiers"], "0");
    }
}
