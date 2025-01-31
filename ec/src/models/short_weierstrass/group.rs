use ark_serialize::{
    CanonicalDeserialize, CanonicalSerialize, Compress, SerializationError, Valid, Validate,
};
use ark_std::{
    borrow::Borrow,
    fmt::{Debug, Display, Formatter, Result as FmtResult},
    hash::{Hash, Hasher},
    io::{Read, Write},
    ops::{Add, AddAssign, Mul, MulAssign, Neg, Sub, SubAssign},
    rand::{
        distributions::{Distribution, Standard},
        Rng,
    },
    vec::Vec,
    One, Zero,
};

use ark_ff::{fields::Field, PrimeField, ToConstraintField, UniformRand};

use zeroize::Zeroize;

#[cfg(feature = "parallel")]
use rayon::prelude::*;

use super::{Affine, SWCurveConfig};
use crate::{
    scalar_mul::{variable_base::VariableBaseMSM, ScalarMul},
    AffineRepr, CurveGroup, Group,
};

/// Jacobian coordinates for a point on an elliptic curve in short Weierstrass
/// form, over the base field `P::BaseField`. This struct implements arithmetic
/// via the Jacobian formulae
#[derive(Derivative)]
#[derivative(Copy(bound = "P: SWCurveConfig"), Clone(bound = "P: SWCurveConfig"))]
#[must_use]
pub struct Projective<P: SWCurveConfig> {
    /// `X / Z` projection of the affine `X`
    pub x: P::BaseField,
    /// `Y / Z` projection of the affine `Y`
    pub y: P::BaseField,
    /// Projective multiplicative inverse. Will be `0` only at infinity.
    pub z: P::BaseField,
}

impl<P: SWCurveConfig> Display for Projective<P> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{}", Affine::from(*self))
    }
}

impl<P: SWCurveConfig> Debug for Projective<P> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self.is_zero() {
            true => write!(f, "infinity"),
            false => write!(f, "({}, {}, {})", self.x, self.y, self.z),
        }
    }
}

impl<P: SWCurveConfig> Eq for Projective<P> {}
impl<P: SWCurveConfig> PartialEq for Projective<P> {
    fn eq(&self, other: &Self) -> bool {
        if self.is_zero() {
            return other.is_zero();
        }

        if other.is_zero() {
            return false;
        }

        // The points (X, Y, Z) and (X', Y', Z')
        // are equal when (X * Z^2) = (X' * Z'^2)
        // and (Y * Z^3) = (Y' * Z'^3).
        let z1z1 = self.z.square();
        let z2z2 = other.z.square();

        if self.x * &z2z2 != other.x * &z1z1 {
            false
        } else {
            self.y * &(z2z2 * &other.z) == other.y * &(z1z1 * &self.z)
        }
    }
}

impl<P: SWCurveConfig> PartialEq<Affine<P>> for Projective<P> {
    fn eq(&self, other: &Affine<P>) -> bool {
        *self == other.into_group()
    }
}

impl<P: SWCurveConfig> Hash for Projective<P> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.into_affine().hash(state)
    }
}

impl<P: SWCurveConfig> Distribution<Projective<P>> for Standard {
    /// Generates a uniformly random instance of the curve.
    #[inline]
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> Projective<P> {
        loop {
            let x = P::BaseField::rand(rng);
            let greatest = rng.gen();

            if let Some(p) = Affine::get_point_from_x_unchecked(x, greatest) {
                return p.mul_by_cofactor_to_group();
            }
        }
    }
}

impl<P: SWCurveConfig> Default for Projective<P> {
    #[inline]
    fn default() -> Self {
        Self::zero()
    }
}

impl<P: SWCurveConfig> Projective<P> {
    /// Constructs a new group element without checking whether the coordinates
    /// specify a point in the subgroup.
    pub const fn new_unchecked(x: P::BaseField, y: P::BaseField, z: P::BaseField) -> Self {
        Self { x, y, z }
    }

    /// Constructs a new group element in a way while enforcing that points are in
    /// the prime-order subgroup.
    pub fn new(x: P::BaseField, y: P::BaseField, z: P::BaseField) -> Self {
        let p = Self::new_unchecked(x, y, z).into_affine();
        assert!(p.is_on_curve());
        assert!(p.is_in_correct_subgroup_assuming_on_curve());
        p.into()
    }
}

impl<P: SWCurveConfig> Zeroize for Projective<P> {
    fn zeroize(&mut self) {
        self.x.zeroize();
        self.y.zeroize();
        self.z.zeroize();
    }
}

impl<P: SWCurveConfig> Zero for Projective<P> {
    /// Returns the point at infinity, which always has Z = 0.
    #[inline]
    fn zero() -> Self {
        Self::new_unchecked(
            P::BaseField::one(),
            P::BaseField::one(),
            P::BaseField::zero(),
        )
    }

    /// Checks whether `self.z.is_zero()`.
    #[inline]
    fn is_zero(&self) -> bool {
        self.z.is_zero()
    }
}

impl<P: SWCurveConfig> Group for Projective<P> {
    type ScalarField = P::ScalarField;

    #[inline]
    fn generator() -> Self {
        Affine::generator().into()
    }

    /// Sets `self = 2 * self`. Note that Jacobian formulae are incomplete, and
    /// so doubling cannot be computed as `self + self`. Instead, this
    /// implementation uses the following specialized doubling formulae:
    /// * [`P::A` is zero](http://www.hyperelliptic.org/EFD/g1p/auto-shortw-jacobian-0.html#doubling-dbl-2009-l)
    /// * [`P::A` is not zero](https://www.hyperelliptic.org/EFD/g1p/auto-shortw-jacobian.html#doubling-dbl-2007-bl)
    fn double_in_place(&mut self) -> &mut Self {
        if self.is_zero() {
            return self;
        }

        if P::COEFF_A.is_zero() {
            // A = X1^2
            let mut a = self.x.square();

            // B = Y1^2
            let b = self.y.square();

            // C = B^2
            let mut c = b.square();

            // D = 2*((X1+B)2-A-C)
            let d = ((self.x + &b).square() - &a - &c).double();

            // E = 3*A
            let e = a + &*a.double_in_place();

            // F = E^2
            let f = e.square();

            // Z3 = 2*Y1*Z1
            self.z *= &self.y;
            self.z.double_in_place();

            // X3 = F-2*D
            self.x = f - &d.double();

            // Y3 = E*(D-X3)-8*C
            self.y = (d - &self.x) * &e - &*c.double_in_place().double_in_place().double_in_place();
            self
        } else {
            // http://www.hyperelliptic.org/EFD/g1p/auto-shortw-jacobian-0.html#doubling-dbl-2009-l
            // XX = X1^2
            let xx = self.x.square();

            // YY = Y1^2
            let yy = self.y.square();

            // YYYY = YY^2
            let mut yyyy = yy.square();

            // ZZ = Z1^2
            let zz = self.z.square();

            // S = 2*((X1+YY)^2-XX-YYYY)
            let s = ((self.x + &yy).square() - &xx - &yyyy).double();

            // M = 3*XX+a*ZZ^2
            let m = xx + xx.double() + P::mul_by_a(zz.square());

            // T = M^2-2*S
            let t = m.square() - &s.double();

            // X3 = T
            self.x = t;
            // Y3 = M*(S-T)-8*YYYY
            let old_y = self.y;
            self.y = m * &(s - &t) - &*yyyy.double_in_place().double_in_place().double_in_place();
            // Z3 = (Y1+Z1)^2-YY-ZZ
            self.z = (old_y + &self.z).square() - &yy - &zz;
            self
        }
    }

    #[inline]
    fn mul_bigint(&self, other: impl AsRef<[u64]>) -> Self {
        P::mul_projective(&self, other.as_ref())
    }
}

impl<P: SWCurveConfig> CurveGroup for Projective<P> {
    type Config = P;
    type BaseField = P::BaseField;
    type Affine = Affine<P>;
    type FullGroup = Affine<P>;

    /// Normalizes a slice of projective elements so that
    /// conversion to affine is cheap.
    ///
    /// In more detail, this method converts a curve point in Jacobian
    /// coordinates (x, y, z) into an equivalent representation (x/z^2,
    /// y/z^3, 1).
    ///
    /// For `N = v.len()`, this costs 1 inversion + 6N field multiplications + N
    /// field squarings.
    ///
    /// (Where batch inversion comprises 3N field multiplications + 1 inversion
    /// of these operations)
    #[inline]
    fn normalize_batch(v: &[Self]) -> Vec<Self::Affine> {
        let mut z_s = v.iter().map(|g| g.z).collect::<Vec<_>>();
        ark_ff::batch_inversion(&mut z_s);

        // Perform affine transformations
        ark_std::cfg_iter!(v)
            .zip(z_s)
            .map(|(g, z)| match g.is_zero() {
                true => Affine::identity(),
                false => {
                    let z2 = z.square();
                    let x = g.x * z2;
                    let y = g.y * z2 * z;
                    Affine::new_unchecked(x, y)
                },
            })
            .collect()
    }
}

impl<P: SWCurveConfig> Neg for Projective<P> {
    type Output = Self;

    #[inline]
    fn neg(mut self) -> Self {
        self.y = -self.y;
        self
    }
}

impl<P: SWCurveConfig, T: Borrow<Affine<P>>> AddAssign<T> for Projective<P> {
    fn add_assign(&mut self, other: T) {
        let other = other.borrow();
        if let Some((&other_x, &other_y)) = other.xy() {
            if self.is_zero() {
                self.x = other_x;
                self.y = other_y;
                self.z = P::BaseField::one();
                return;
            }

            // Z1Z1 = Z1^2
            let z1z1 = self.z.square();

            // U2 = X2*Z1Z1
            let u2 = z1z1 * other_x;

            // S2 = Y2*Z1*Z1Z1
            let s2 = (self.z * other_y) * &z1z1;

            if self.x == u2 && self.y == s2 {
                // The two points are equal, so we double.
                self.double_in_place();
            } else {
                // If we're adding -a and a together, self.z becomes zero as H becomes zero.

                // H = U2-X1
                let h = u2 - &self.x;

                // HH = H^2
                let hh = h.square();

                // I = 4*HH
                let mut i = hh;
                i.double_in_place().double_in_place();

                // J = H*I
                let j = h * &i;

                // r = 2*(S2-Y1)
                let r = (s2 - &self.y).double();

                // V = X1*I
                let v = self.x * &i;

                // X3 = r^2 - J - 2*V
                self.x = r.square();
                self.x -= &j;
                self.x -= &v.double();

                // Y3 = r*(V-X3)-2*Y1*J
                self.y = P::BaseField::sum_of_products(&[r, -self.y.double()], &[(v - &self.x), j]);

                // Z3 = (Z1+H)^2-Z1Z1-HH
                self.z += &h;
                self.z.square_in_place();
                self.z -= &z1z1;
                self.z -= &hh;
            }
        }
    }
}

impl<P: SWCurveConfig, T: Borrow<Affine<P>>> Add<T> for Projective<P> {
    type Output = Self;
    fn add(mut self, other: T) -> Self {
        let other = other.borrow();
        self += other;
        self
    }
}

impl<P: SWCurveConfig, T: Borrow<Affine<P>>> SubAssign<T> for Projective<P> {
    fn sub_assign(&mut self, other: T) {
        *self += -(*other.borrow());
    }
}

impl<P: SWCurveConfig, T: Borrow<Affine<P>>> Sub<T> for Projective<P> {
    type Output = Self;
    fn sub(mut self, other: T) -> Self {
        self -= other.borrow();
        self
    }
}

ark_ff::impl_additive_ops_from_ref!(Projective, SWCurveConfig);

impl<'a, P: SWCurveConfig> Add<&'a Self> for Projective<P> {
    type Output = Self;

    #[inline]
    fn add(mut self, other: &'a Self) -> Self {
        self += other;
        self
    }
}

impl<'a, P: SWCurveConfig> AddAssign<&'a Self> for Projective<P> {
    fn add_assign(&mut self, other: &'a Self) {
        if self.is_zero() {
            *self = *other;
            return;
        }

        if other.is_zero() {
            return;
        }

        // http://www.hyperelliptic.org/EFD/g1p/auto-shortw-jacobian-0.html#addition-add-2007-bl
        // Works for all curves.

        // Z1Z1 = Z1^2
        let z1z1 = self.z.square();

        // Z2Z2 = Z2^2
        let z2z2 = other.z.square();

        // U1 = X1*Z2Z2
        let u1 = self.x * &z2z2;

        // U2 = X2*Z1Z1
        let u2 = other.x * &z1z1;

        // S1 = Y1*Z2*Z2Z2
        let s1 = self.y * &other.z * &z2z2;

        // S2 = Y2*Z1*Z1Z1
        let s2 = other.y * &self.z * &z1z1;

        if u1 == u2 && s1 == s2 {
            // The two points are equal, so we double.
            self.double_in_place();
        } else {
            // If we're adding -a and a together, self.z becomes zero as H becomes zero.

            // H = U2-U1
            let h = u2 - &u1;

            // I = (2*H)^2
            let i = (h.double()).square();

            // J = H*I
            let j = h * &i;

            // r = 2*(S2-S1)
            let r = (s2 - &s1).double();

            // V = U1*I
            let v = u1 * &i;

            // X3 = r^2 - J - 2*V
            self.x = r.square() - &j - &(v.double());

            // Y3 = r*(V - X3) - 2*S1*J
            self.y = P::BaseField::sum_of_products(&[r, -s1.double()], &[(v - &self.x), j]);

            // Z3 = ((Z1+Z2)^2 - Z1Z1 - Z2Z2)*H
            self.z = ((self.z + &other.z).square() - &z1z1 - &z2z2) * &h;
        }
    }
}

impl<'a, P: SWCurveConfig> Sub<&'a Self> for Projective<P> {
    type Output = Self;

    #[inline]
    fn sub(mut self, other: &'a Self) -> Self {
        self -= other;
        self
    }
}

impl<'a, P: SWCurveConfig> SubAssign<&'a Self> for Projective<P> {
    fn sub_assign(&mut self, other: &'a Self) {
        *self += &(-(*other));
    }
}

impl<P: SWCurveConfig, T: Borrow<P::ScalarField>> MulAssign<T> for Projective<P> {
    fn mul_assign(&mut self, other: T) {
        *self = self.mul_bigint(other.borrow().into_bigint())
    }
}

impl<'a, P: SWCurveConfig, T: Borrow<P::ScalarField>> Mul<T> for Projective<P> {
    type Output = Self;

    #[inline]
    fn mul(mut self, other: T) -> Self {
        self *= other;
        self
    }
}

// The affine point X, Y is represented in the Jacobian
// coordinates with Z = 1.
impl<P: SWCurveConfig> From<Affine<P>> for Projective<P> {
    #[inline]
    fn from(p: Affine<P>) -> Projective<P> {
        p.xy().map_or(Projective::zero(), |(&x, &y)| Self {
            x,
            y,
            z: P::BaseField::one(),
        })
    }
}

impl<P: SWCurveConfig> CanonicalSerialize for Projective<P> {
    #[inline]
    fn serialize_with_mode<W: Write>(
        &self,
        writer: W,
        compress: Compress,
    ) -> Result<(), SerializationError> {
        let aff = Affine::<P>::from(*self);
        P::serialize_with_mode(&aff, writer, compress)
    }

    #[inline]
    fn serialized_size(&self, compress: Compress) -> usize {
        P::serialized_size(compress)
    }
}

impl<P: SWCurveConfig> Valid for Projective<P> {
    fn check(&self) -> Result<(), SerializationError> {
        self.into_affine().check()
    }

    fn batch_check<'a>(
        batch: impl Iterator<Item = &'a Self> + Send,
    ) -> Result<(), SerializationError>
    where
        Self: 'a,
    {
        let batch = batch.copied().collect::<Vec<_>>();
        let batch = Self::normalize_batch(&batch);
        Affine::batch_check(batch.iter())
    }
}

impl<P: SWCurveConfig> CanonicalDeserialize for Projective<P> {
    fn deserialize_with_mode<R: Read>(
        reader: R,
        compress: Compress,
        validate: Validate,
    ) -> Result<Self, SerializationError> {
        let aff = P::deserialize_with_mode(reader, compress, validate)?;
        Ok(aff.into())
    }
}

impl<M: SWCurveConfig, ConstraintF: Field> ToConstraintField<ConstraintF> for Projective<M>
where
    M::BaseField: ToConstraintField<ConstraintF>,
{
    #[inline]
    fn to_field_elements(&self) -> Option<Vec<ConstraintF>> {
        Affine::from(*self).to_field_elements()
    }
}

impl<P: SWCurveConfig> ScalarMul for Projective<P> {
    type MulBase = Affine<P>;

    fn batch_convert_to_mul_base(bases: &[Self]) -> Vec<Self::MulBase> {
        Self::normalize_batch(bases)
    }
}

impl<P: SWCurveConfig> VariableBaseMSM for Projective<P> {}

impl<P: SWCurveConfig, T: Borrow<Affine<P>>> core::iter::Sum<T> for Projective<P> {
    fn sum<I: Iterator<Item = T>>(iter: I) -> Self {
        iter.fold(Projective::zero(), |sum, x| sum + x.borrow())
    }
}
