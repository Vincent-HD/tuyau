use std::io::Cursor;

use pipewire::spa::{
    self,
    param::ParamType,
    pod::{
        Object, Pod, Property, Value, ValueArray, deserialize::PodDeserializer,
        serialize::PodSerializer,
    },
    utils::SpaTypes,
};

/// Fields of a `Props` param we care about; absent ones are `None`.
#[derive(Debug, Default, PartialEq)]
pub struct Props {
    pub channel_volumes: Option<Vec<f32>>,
    pub mute: Option<bool>,
}

pub fn parse_props(pod: &Pod) -> Props {
    let Ok((_, Value::Object(object))) = PodDeserializer::deserialize_any_from(pod.as_bytes())
    else {
        return Props::default();
    };
    let mut props = Props::default();
    for property in object.properties {
        match (property.key, property.value) {
            (spa::sys::SPA_PROP_channelVolumes, Value::ValueArray(ValueArray::Float(volumes))) => {
                props.channel_volumes = Some(volumes);
            }
            (spa::sys::SPA_PROP_mute, Value::Bool(mute)) => props.mute = Some(mute),
            _ => {}
        }
    }
    props
}

pub fn serialize_props(props: &Props) -> Vec<u8> {
    let mut properties = Vec::new();
    if let Some(volumes) = &props.channel_volumes {
        properties.push(Property::new(
            spa::sys::SPA_PROP_channelVolumes,
            Value::ValueArray(ValueArray::Float(volumes.clone())),
        ));
    }
    if let Some(mute) = props.mute {
        properties.push(Property::new(spa::sys::SPA_PROP_mute, Value::Bool(mute)));
    }
    let value = Value::Object(Object {
        type_: SpaTypes::ObjectParamProps.as_raw(),
        id: ParamType::Props.as_raw(),
        properties,
    });
    PodSerializer::serialize(Cursor::new(Vec::new()), &value)
        .expect("props pod serializes into memory")
        .0
        .into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn props_round_trip() {
        let props = Props {
            channel_volumes: Some(vec![0.5, 0.25]),
            mute: Some(true),
        };
        let bytes = serialize_props(&props);
        let pod = Pod::from_bytes(&bytes).expect("serialized bytes are a pod");
        assert_eq!(parse_props(pod), props);
    }
}
