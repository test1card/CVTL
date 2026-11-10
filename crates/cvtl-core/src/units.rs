//! SI quantities exchanged between the thermal model and material curves.
//!
//! These types require finite values. Context-specific constraints such as positive
//! mass and the temperature interval of a material belong to the model.

macro_rules! quantity {
    ($name:ident, $unit:literal) => {
        #[doc = $unit]
        #[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
        pub struct $name(f64);

        impl $name {
            pub fn new(value: f64) -> Option<Self> {
                value.is_finite().then_some(Self(value))
            }

            pub fn get(self) -> f64 {
                self.0
            }
        }
    };
}

quantity!(Kelvin, "Temperature in K.");
quantity!(Kilograms, "Mass in kg.");
quantity!(Joules, "Energy in J.");
quantity!(Watts, "Power or signed heat flow in W.");
quantity!(Seconds, "Time interval in s.");
quantity!(Meters, "Length in m.");
quantity!(SquareMeters, "Area in m².");
quantity!(WattsPerKelvin, "Thermal conductance in W/K.");
quantity!(KelvinPerSecond, "Temperature rate in K/s.");
quantity!(JoulesPerKelvin, "Heat capacity in J/K.");
quantity!(JoulesPerKilogram, "Specific enthalpy in J/kg.");
quantity!(
    JoulesPerKilogramKelvin,
    "Specific heat capacity in J/(kg·K)."
);
quantity!(WattsPerMeterKelvin, "Thermal conductivity in W/(m·K).");
quantity!(
    WattsPerMeter,
    "Integral of conductivity over temperature in W/m."
);
