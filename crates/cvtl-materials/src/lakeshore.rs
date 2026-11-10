use super::{MaterialError, ThermalConductivityCurve};
use cvtl_core::units::{Kelvin, WattsPerMeterKelvin};

pub const LAKESHORE_WIRE_SOURCE: &str = "https://www.lakeshore.com/products/categories/specification/temperature-products/cryogenic-accessories/cryogenic-wire";

/// Tabulated conductivity of nominal wire materials, linear between published knots.
pub fn lakeshore_wire_conductivity(id: &str) -> Result<ThermalConductivityCurve, MaterialError> {
    let column = match id {
        "lakeshore_copper" => 1,
        "lakeshore_phosphor_bronze" => 2,
        "lakeshore_nichrome" => 3,
        "lakeshore_manganin" => 4,
        _ => return Err(MaterialError::UnknownMaterial),
    };
    let mut points = Vec::new();
    for row in include_str!("../../../data/materials/lakeshore-wire.csv")
        .lines()
        .skip(1)
    {
        let mut fields = row.split(',');
        let t = fields.next().ok_or(MaterialError::InvalidCurve)?;
        let k = fields.nth(column - 1).ok_or(MaterialError::InvalidCurve)?;
        if k.is_empty() {
            continue;
        }
        let t = t
            .parse()
            .ok()
            .and_then(Kelvin::new)
            .ok_or(MaterialError::InvalidCurve)?;
        let k = k
            .parse()
            .ok()
            .and_then(WattsPerMeterKelvin::new)
            .ok_or(MaterialError::InvalidCurve)?;
        points.push((t, k));
    }
    ThermalConductivityCurve::new(points)
}
