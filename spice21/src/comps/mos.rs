//!
//! # MOS Solvers Module
//!
//! Shared Mos
//!

use num::Complex;
use serde::{Deserialize, Serialize};
use std::convert::From;
use std::ops::{Index, IndexMut};

use super::consts;
use super::{make_matrix_elem, Component};
use crate::analysis::{AnalysisInfo, ChargeInteg, Options, Stamps, TranState, VarIndex, Variables};
use crate::defs::DefPtr;
use crate::sparse21::{Eindex, Matrix};
use crate::{analysis, proto, SpNum};

/// Mos Terminals, in SPICE order: d, g, s, b
#[derive(Clone, Copy)]
pub enum MosTerm {
    D = 0,
    G = 1,
    S = 2,
    B = 3,
}
#[derive(Default)]
pub struct MosPorts<T> {
    pub d: T,
    pub g: T,
    pub s: T,
    pub b: T,
}
/// Index MosPorts by the `MosTerm` enum
impl<T> Index<MosTerm> for MosPorts<T> {
    type Output = T;
    fn index(&self, t: MosTerm) -> &T {
        use MosTerm::{B, D, G, S};
        match t {
            D => &self.d,
            G => &self.g,
            S => &self.s,
            B => &self.b,
        }
    }
}
/// Very fun conversion from four-element arrays into MosPorts of `From`-able types.
impl<S, T: Clone + Into<S>> From<[T; 4]> for MosPorts<S> {
    fn from(n: [T; 4]) -> MosPorts<S> {
        return MosPorts {
            d: n[0].clone().into(),
            g: n[1].clone().into(),
            s: n[2].clone().into(),
            b: n[3].clone().into(),
        };
    }
}
/// Even more fun conversion from four-element tuples into MosPorts of `From`-able types.
/// Note in this case, each of the four elements can be of distinct types.
impl<S, T: Clone + Into<S>, U: Clone + Into<S>, V: Clone + Into<S>, W: Clone + Into<S>> From<(T, U, V, W)> for MosPorts<S> {
    fn from(n: (T, U, V, W)) -> MosPorts<S> {
        return MosPorts {
            d: n.0.clone().into(),
            g: n.1.clone().into(),
            s: n.2.clone().into(),
            b: n.3.clone().into(),
        };
    }
}

#[derive(Default)]
pub(crate) struct Mos1MatrixPointers([[Option<Eindex>; 6]; 6]);

impl Index<(Mos1Var, Mos1Var)> for Mos1MatrixPointers {
    type Output = Option<Eindex>;
    fn index(&self, ts: (Mos1Var, Mos1Var)) -> &Option<Eindex> {
        &self.0[ts.0 as usize][ts.1 as usize]
    }
}
impl IndexMut<(Mos1Var, Mos1Var)> for Mos1MatrixPointers {
    fn index_mut(&mut self, ts: (Mos1Var, Mos1Var)) -> &mut Self::Output {
        &mut self.0[ts.0 as usize][ts.1 as usize]
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum MosType {
    NMOS,
    PMOS,
}
impl Default for MosType {
    fn default() -> MosType {
        MosType::NMOS
    }
}
impl MosType {
    /// Polarity Function
    /// The very common need to negate values for PMOS, and leave NMOS unchanged.
    pub fn p(&self) -> f64 {
        match self {
            MosType::PMOS => -1.0,
            MosType::NMOS => 1.0,
        }
    }
}

/// Mos Level 1 Model Parameters
#[derive(Clone)]
pub struct Mos1Model {
    pub mos_type: MosType,
    pub vt0: f64,
    pub kp: f64,
    pub gamma: f64,
    pub cox_per_area: f64,
    pub phi: f64,
    pub lambda: f64,
    pub cbd: f64,
    pub cbs: f64,
    pub is: f64,
    pub pb: f64,
    pub cgso: f64,
    pub cgdo: f64,
    pub cgbo: f64,
    pub cj: f64,
    pub mj: f64,
    pub cjsw: f64,
    pub mjsw: f64,
    pub js: f64,
    pub tox: f64,
    pub ld: f64,
    pub fc: f64,
    pub tnom: f64,
    pub kf: f64,
    pub af: f64,
    pub rd: Option<f64>,
    pub rs: Option<f64>,
    pub rsh: Option<f64>,
}
impl Mos1Model {
    pub(crate) fn resolve(specs: &proto::Mos1Model) -> Self {
        use consts::{KB, KB_OVER_Q, KELVIN_TO_C, Q, SIO2_PERMITTIVITY, TEMP_REF};

        // Convert from Proto-encoded enum form
        let mos_type = if specs.mos_type == 1 { MosType::PMOS } else { MosType::NMOS };

        // Nominal temperature. C to Kelvin conversion happens right here
        let tnom = if let Some(val) = specs.tnom { val + KELVIN_TO_C } else { TEMP_REF };
        let fact1 = tnom / TEMP_REF;
        let vtnom = tnom * KB_OVER_Q;
        let kt1 = KB * tnom;
        let egfet1 = 1.16 - (7.02e-4 * tnom.powi(2)) / (tnom + 1108.0);
        let arg1 = -egfet1 / 2.0 / kt1 + 1.1150877 / (KB * 2.0 * TEMP_REF);
        let pbfact1 = -2.0 * vtnom * (1.5 * fact1.ln() + Q * arg1);

        // Parameter defaults take very different tracks depending whether `tox` is specified.
        // First, the no-tox cases:
        let mut cox_per_area = 0.0;
        let mut vt0 = if let Some(val) = specs.vt0 { val } else { 0.0 };
        let mut kp = if let Some(val) = specs.kp { val } else { 2.0e-5 };
        let mut phi = if let Some(val) = specs.phi { val } else { 0.6 };
        let mut gamma = if let Some(val) = specs.gamma { val } else { 0.0 };

        // Now, each of these are updated in the (typical) case in which `tox` is provided
        if let Some(tox) = specs.tox {
            cox_per_area = SIO2_PERMITTIVITY / tox;
            if specs.kp.is_none() {
                let u0 = if let Some(val) = specs.u0 { val } else { 600.0 };
                kp = u0 * cox_per_area * 1e-4 /*(m**2/cm**2)*/;
            };
            // Substrate doping
            if let Some(nsub) = specs.nsub {
                if nsub * 1e6 /*(cm**3/m**3)*/ <= 1.45e16 {
                    // FIXME: do this check for no-tox too?
                    panic!("Invalid Mos1 Substrate Doping nsub < ni (1.45e16)")
                }
                if specs.phi.is_none() {
                    phi = 2.0 * vtnom * (nsub*1e6/*(cm**3/m**3)*//1.45e16).ln();
                    phi = phi.max(0.1);
                }
                // Gate-type manipulations
                let fermis = mos_type.p() * 0.5 * phi;
                let mut wkfng = 3.2;
                let gate_type: f64 = if let Some(val) = specs.tpg {
                    if val > 1 || val < -1 {
                        panic!("Invalid Mos1 tps: {}", val);
                    }
                    val as f64
                } else {
                    1.0
                };
                if gate_type != 0.0 {
                    let fermig = mos_type.p() * gate_type * 0.5 * egfet1;
                    wkfng = 3.25 + 0.5 * egfet1 - fermig;
                }
                if specs.gamma.is_none() {
                    gamma = (2.0 * 11.70 * 8.854214871e-12 * Q * nsub * 1e6/*(cm**3/m**3)*/).sqrt() / cox_per_area;
                }
                if specs.vt0.is_none() {
                    let nss = if let Some(val) = specs.nss { val } else { 0.0 };
                    let wkfngs = wkfng - (3.25 + 0.5 * egfet1 + fermis);
                    let vfb = wkfngs - nss *1e4 /*(cm**2/m**2)*/ *Q / cox_per_area;
                    vt0 = vfb + mos_type.p() * (gamma * (phi).sqrt() + phi);
                }
            }
        }

        Self {
            mos_type, // Calculated above
            vt0,
            kp,
            cox_per_area,
            gamma,
            phi,
            tnom,
            lambda: if let Some(val) = specs.lambda { val } else { 0.0 }, // More-straightforward default values
            pb: if let Some(val) = specs.pb { val } else { 0.8 },
            cbd: if let Some(val) = specs.cbd { val } else { 0.0 },
            cbs: if let Some(val) = specs.cbs { val } else { 0.0 },
            cgso: if let Some(val) = specs.cgso { val } else { 0.0 },
            cgdo: if let Some(val) = specs.cgdo { val } else { 0.0 },
            cgbo: if let Some(val) = specs.cgbo { val } else { 0.0 },
            cj: if let Some(val) = specs.cj { val } else { 0.0 },
            cjsw: if let Some(val) = specs.cjsw { val } else { 0.0 },
            mj: if let Some(val) = specs.mj { val } else { 0.5 },
            mjsw: if let Some(val) = specs.mjsw { val } else { 0.5 },
            is: if let Some(val) = specs.is { val } else { 1.0e-14 },
            js: if let Some(val) = specs.js { val } else { 1.0e-8 }, // FIXME
            tox: if let Some(val) = specs.tox { val } else { 1.0e-7 },
            ld: if let Some(val) = specs.ld { val } else { 0.0 },
            fc: if let Some(val) = specs.fc { val } else { 0.5 },
            kf: if let Some(val) = specs.kf { val } else { 0.0 },
            af: if let Some(val) = specs.af { val } else { 1.0 },
            rd: specs.rd, // Options
            rs: specs.rs,
            rsh: specs.rsh,
        }
    }
    /// MosType polarity accessor
    pub(crate) fn p(&self) -> f64 {
        self.mos_type.p()
    }
}
impl Default for Mos1Model {
    fn default() -> Self {
        Self::resolve(&proto::Mos1Model::default())
    }
}

/// Mos Level 1 Instance Parameters
#[derive(Clone, Copy, Debug)]
pub struct Mos1InstanceParams {
    m: f64,
    l: f64,
    w: f64,
    a_d: f64,
    a_s: f64,
    pd: f64,
    ps: f64,
    nrd: f64,
    nrs: f64,
    temp: Option<f64>,
    // FIXME: maybe even more explicitly ignore these
    // dtemp: Option<f64>,
    // off: bool,
    // icvds: f64,
    // icvgs: f64,
    // icvbs: f64,
    // ic: f64,
}
impl Mos1InstanceParams {
    pub(crate) fn resolve(specs: &proto::Mos1InstParams) -> Self {
        Mos1InstanceParams {
            m: if let Some(val) = specs.m { val } else { 0.0 },
            l: if let Some(val) = specs.l { val } else { 1e-6 },
            w: if let Some(val) = specs.w { val } else { 1e-6 },
            a_d: if let Some(val) = specs.a_d { val } else { 1e-12 },
            a_s: if let Some(val) = specs.a_s { val } else { 1e-12 },
            pd: if let Some(val) = specs.pd { val } else { 1e-6 },
            ps: if let Some(val) = specs.ps { val } else { 1e-6 },
            nrd: if let Some(val) = specs.nrd { val } else { 1.0 },
            nrs: if let Some(val) = specs.nrs { val } else { 1.0 },
            temp: specs.temp
            // dtemp: if let Some(val) = specs.dtemp { val } else { 0.0 },
            // icvds: if let Some(val) = specs.icvds { val } else { 0.0 },
            // icvgs: if let Some(val) = specs.icvgs { val } else { 0.0 },
            // icvbs: if let Some(val) = specs.icvbs { val } else { 0.0 },
            // ic: if let Some(val) = specs.ic { val } else { 0.0 },
            // off: specs.off,
        }
    }
}
impl Default for Mos1InstanceParams {
    fn default() -> Self {
        Self::resolve(&proto::Mos1InstParams::default())
    }
}

/// Mos1 Internal "Parameters", derived at instance-construction
/// and updated only on changes in temperature
#[derive(Default)]
pub(crate) struct Mos1InternalParams {
    pub(crate) temp: f64,
    pub(crate) vtherm: f64,
    pub(crate) vt0_t: f64,
    pub(crate) kp_t: f64,
    pub(crate) phi_t: f64,
    pub(crate) beta: f64,
    pub(crate) cox: f64,
    pub(crate) cgs_ov: f64,
    pub(crate) cgd_ov: f64,
    pub(crate) cgb_ov: f64,
    pub(crate) leff: f64,
    pub(crate) drain_junc: MosJunction,
    pub(crate) source_junc: MosJunction,
    pub(crate) grd: f64,
    pub(crate) grs: f64,
}
impl Mos1InternalParams {
    /// Calculate derived parameters from instance and model parameters
    fn derive(model: &Mos1Model, inst: &Mos1InstanceParams, opts: &Options) -> Mos1InternalParams {
        if let Some(t) = inst.temp {
            panic!("Mos1 Instance Temperatures Are Not Supported");
        }
        let temp = opts.temp; // Note: in Kelvin

        // Nominal temperature params (note: repeated calcs from Model::derive)
        use consts::{KB, KB_OVER_Q, Q, TEMP_REF};
        let fact1 = model.tnom / TEMP_REF;
        let vtnom = model.tnom * KB_OVER_Q;
        let kt1 = KB * model.tnom;
        let egfet1 = 1.16 - (7.02e-4 * model.tnom.powi(2)) / (model.tnom + 1108.0);
        let arg1 = -egfet1 / 2.0 / kt1 + 1.1150877 / (KB * 2.0 * TEMP_REF);
        let pbfact1 = -2.0 * vtnom * (1.5 * fact1.ln() + Q * arg1);

        // Instance temperature params
        let kt = temp * KB;
        let vtherm = temp * KB_OVER_Q;
        let temp_ratio = temp / model.tnom;
        let fact2 = temp / TEMP_REF;
        let egfet = 1.16 - (7.02e-4 * temp.powi(2)) / (temp + 1108.0);
        let arg = -egfet / 2.0 / kt + 1.1150877 / (KB * 2.0 * TEMP_REF);
        let pbfact = -2.0 * vtherm * (1.5 * fact2.ln() + Q * arg);

        // Effective Length
        let leff = inst.l - 2.0 * model.ld;
        if leff < 0.0 {
            panic!("Mos1 Effective Length < 0");
        }

        let phio = (model.phi - pbfact1) / fact1;
        let phi_t = fact2 * phio + pbfact;
        let vbi_t = model.vt0 - model.p() * (model.gamma * model.phi.sqrt()) + 0.5 * (egfet1 - egfet) + model.p() * 0.5 * (phi_t - model.phi);
        let vt0_t = vbi_t + model.p() * model.gamma * phi_t.sqrt();
        let isat_t = model.is * (-egfet / vtherm + egfet1 / vtnom).exp();
        let jsat_t = model.js * (-egfet / vtherm + egfet1 / vtnom).exp();

        let pbo = (model.pb - pbfact1) / fact1;
        let gmaold = (model.pb - pbo) / pbo;
        let capfact = 1.0 / (1.0 + model.mj * (4e-4 * (model.tnom - TEMP_REF) - gmaold));
        let mut cbd_t = model.cbd * capfact;
        let mut cbs_t = model.cbs * capfact;
        let mut cj_t = model.cj * capfact;
        let capfact = 1.0 / (1.0 + model.mjsw * (4e-4 * (model.tnom - TEMP_REF) - gmaold));
        let mut cjsw_t = model.cjsw * capfact;
        let bulkpot_t = fact2 * pbo + pbfact;
        let gmanew = (bulkpot_t - pbo) / pbo;
        let capfact = 1.0 / (1.0 + model.mj * (4e-4 * (temp - TEMP_REF) - gmanew));
        cbd_t *= capfact;
        cbs_t *= capfact;
        cj_t *= capfact;
        let capfact = 1.0 / (1.0 + model.mjsw * (4e-4 * (temp - TEMP_REF) - gmanew));
        cjsw_t *= capfact;

        // S/D Junction Params
        let depletion_threshold = model.fc * bulkpot_t;
        let arg = 1.0 - model.fc;
        let sarg = ((-model.mj) * arg.ln()).exp();
        let sargsw = ((-model.mjsw) * arg.ln()).exp();
        let use_default_isat: bool = jsat_t == 0.0 || inst.a_d == 0.0 || inst.a_s == 0.0;

        // MosJunction construction-closure
        let junc_new = |area: f64, perim: f64, _sd: SourceDrain| {
            let isat = if use_default_isat { isat_t } else { jsat_t * area };
            let vcrit = vtherm * (vtherm / (consts::SQRT2 * isat)).ln();
            let czb = match _sd {
                SourceDrain::D => match model.cbd {
                    0.0 => cj_t * area,
                    _ => cbd_t,
                },
                SourceDrain::S => match model.cbs {
                    0.0 => cj_t * area,
                    _ => cbs_t,
                },
            };
            let czbsw = cjsw_t * perim;
            let f2 = czb * (1.0 - model.fc * (1.0 + model.mj)) * sarg / arg + czbsw * (1.0 - model.fc * (1.0 + model.mjsw)) * sargsw / arg;
            let f3 = czb * model.mj * sarg / arg / bulkpot_t + czbsw * model.mjsw * sargsw / arg / bulkpot_t;
            let f4 = czb * bulkpot_t * (1.0 - arg * sarg) / (1.0 - model.mj) + czbsw * bulkpot_t * (1.0 - arg * sargsw) / (1.0 - model.mjsw)
                - f3 / 2.0 * (depletion_threshold * depletion_threshold)
                - depletion_threshold * f2;

            MosJunction {
                area,
                isat,
                depletion_threshold,
                bulkpot_t,
                vcrit,
                czb,
                czbsw,
                f2,
                f3,
                f4,
                _sd,
            }
        };

        // Create the source & drain junction params
        let drain_junc = junc_new(inst.a_d, inst.pd, SourceDrain::D);
        let source_junc = junc_new(inst.a_s, inst.ps, SourceDrain::S);

        // Terminal Ohmic Resistances
        let grs = if let Some(r) = model.rs {
            if r <= 0.0 {
                println!("Warning: Mos1 Model with rs <= 0");
                0.0
            } else {
                1.0 / r
            }
        } else if let Some(rsh) = model.rsh {
            if rsh <= 0.0 {
                println!("Warning: Mos1 Model with rsh <= 0");
                0.0
            } else {
                1.0 / rsh / inst.nrs
            }
        } else {
            0.0
        };
        let grd = if let Some(r) = model.rd {
            if r <= 0.0 {
                println!("Warning: Mos1 Model with rd <= 0");
                0.0
            } else {
                1.0 / r
            }
        } else if let Some(rsh) = model.rsh {
            if rsh <= 0.0 {
                println!("Warning: Mos1 Model with rsh <= 0");
                0.0
            } else {
                1.0 / rsh / inst.nrd
            }
        } else {
            0.0
        };

        // Temperature-adjusted transconductance
        let kp_t = model.kp / temp_ratio * temp_ratio.sqrt();
        Mos1InternalParams {
            vt0_t,
            kp_t,
            temp,
            vtherm,
            leff,
            cox: model.cox_per_area * leff * inst.w,
            beta: kp_t * inst.w / leff,
            phi_t,
            drain_junc,
            source_junc,
            cgs_ov: inst.w * model.cgso,
            cgd_ov: inst.w * model.cgdo,
            cgb_ov: leff * model.cgbo,
            grs,
            grd,
        }
    }
}

enum SourceDrain {
    S,
    D,
}
impl Default for SourceDrain {
    fn default() -> Self {
        SourceDrain::S
    }
}
#[derive(Default)]
pub(crate) struct MosJunction {
    area: f64,
    isat: f64,
    depletion_threshold: f64,
    bulkpot_t: f64,
    vcrit: f64,
    czb: f64,
    czbsw: f64,
    f2: f64,
    f3: f64,
    f4: f64,
    _sd: SourceDrain,
}
impl MosJunction {
    /// Charge and Capacitance Calculations
    pub(crate) fn qc(&self, v: f64, model: &Mos1Model) -> (f64, f64) {
        use super::cmath::{exp, log};
        if self.czb == 0.0 && self.czbsw == 0.0 {
            return (0.0, 0.0);
        }
        if v < self.depletion_threshold {
            let arg = 1.0 - v / self.bulkpot_t;
            let sarg = exp(-model.mj * log(arg));
            let sargsw = exp(-model.mjsw * log(arg));
            let q = self.bulkpot_t * (self.czb * (1.0 - arg * sarg) / (1.0 - model.mj) + self.czbsw * (1.0 - arg * sargsw) / (1.0 - model.mjsw));
            let c = self.czb * sarg + self.czbsw * sargsw;
            (q, c)
        } else {
            let q = self.f4 + v * (self.f2 + v * self.f3 / 2.0);
            let c = self.f2 + v * self.f3;
            (q, c)
        }
    }
}

/// Mos1 DC & Transient Operating Point
#[derive(Default, Clone)]
pub(crate) struct Mos1OpPoint {
    ids: f64,
    vgs: f64,
    vds: f64,
    vgd: f64,
    vgb: f64,
    vdb: f64,
    vsb: f64,
    gm: f64,
    gds: f64,
    gmbs: f64,
    gbs: f64,
    gbd: f64,
    cgs: f64,
    cgd: f64,
    cgb: f64,
    cbs: f64,
    cbd: f64,
    reversed: bool,
    tr: Mos1TranState,
}
/// Local structure for transient results,
/// in the form of numerical-integration (conductance, current, rhs)'s
#[derive(Default, Clone)]
struct Mos1TranState {
    gs: ChargeInteg,
    gd: ChargeInteg,
    gb: ChargeInteg,
    bs: ChargeInteg,
    bd: ChargeInteg,
}
/// # Mos1 Node Variables
/// Including internal drain/source nodes
/// for inclusion of terminal resistances.
#[derive(Default)]
pub struct Mos1Vars<T> {
    pub d: T,  // Drain
    pub dp: T, // Internal drain (prime)
    pub g: T,  // Gate
    pub s: T,  // Source
    pub sp: T, // Internal source (prime)
    pub b: T,  // Bulk
}
#[derive(Clone, Copy)]
pub enum Mos1Var {
    D = 0,
    G = 1,
    S = 2,
    B = 3,
    DP = 4,
    SP = 5,
}
impl<T> Index<Mos1Var> for Mos1Vars<T> {
    type Output = T;
    fn index(&self, t: Mos1Var) -> &T {
        use Mos1Var::*;
        match t {
            DP => &self.dp,
            SP => &self.sp,
            D => &self.d,
            G => &self.g,
            S => &self.s,
            B => &self.b,
        }
    }
}
impl Mos1Vars<Option<VarIndex>> {
    pub(crate) fn from<P: Clone + Into<Option<VarIndex>>, T: SpNum>(
        path: String,
        terms: &MosPorts<P>,
        model: &Mos1Model,
        vars: &mut Variables<T>,
    ) -> Self {
        let dp = if model.rd.is_some() || model.rsh.is_some() {
            let name = format!("{}.{}", path, "dp");
            Some(vars.add(name, analysis::VarKind::V))
        } else {
            terms.d.clone().into()
        };
        let sp = if model.rs.is_some() || model.rsh.is_some() {
            let name = format!("{}.{}", path, "sp");
            Some(vars.add(name, analysis::VarKind::V))
        } else {
            terms.s.clone().into()
        };
        Self {
            d: terms.d.clone().into(),
            g: terms.g.clone().into(),
            s: terms.s.clone().into(),
            b: terms.b.clone().into(),
            dp,
            sp,
        }
    }
}
///
/// # Mos Level 1 Solver
///
#[derive(Default)]
pub struct Mos1 {
    pub(crate) model: DefPtr<Mos1Model>,
    pub(crate) intparams: DefPtr<Mos1InternalParams>,
    pub(crate) _params: DefPtr<Mos1InstanceParams>,
    pub(crate) ports: Mos1Vars<Option<VarIndex>>,
    pub(crate) op: Mos1OpPoint,
    pub(crate) guess: Mos1OpPoint,
    pub(crate) matps: Mos1MatrixPointers,
}
impl Mos1 {
    /// Gather the voltages on each of our node-variables from `Variables` `guess`.
    fn vs(&self, vars: &Variables<f64>) -> Mos1Vars<f64> {
        use Mos1Var::{B, D, DP, G, S, SP};
        Mos1Vars {
            d: vars.get(self.ports[D]),
            g: vars.get(self.ports[G]),
            s: vars.get(self.ports[S]),
            b: vars.get(self.ports[B]),
            dp: vars.get(self.ports[DP]),
            sp: vars.get(self.ports[SP]),
        }
    }
    /// Primary action behind dc & transient loading.
    /// Returns calculated "guess" operating point, plus matrix stamps
    fn op_stamp(&self, v: Mos1Vars<f64>, an: &AnalysisInfo, opts: &Options) -> (Mos1OpPoint, Stamps<f64>) {
        let model = &*self.model.read();
        let intp = &*self.intparams.read();
        let gmin = opts.gmin;
        use Mos1Var::{B, D, DP, G, S, SP};
        // Initially factor out polarity of NMOS/PMOS and source/drain swapping
        // All math after this block uses increasing vgs,vds <=> increasing ids,
        // i.e. the polarities typically expressed for NMOS
        let p = model.mos_type.p();
        let reversed = p * (v.d - v.s) < 0.0;
        // FIXME: add inter-step limiting
        let (vd, vs) = if reversed { (v.s, v.d) } else { (v.d, v.s) };
        let vgs = p * (v.g - vs);
        let vgd = p * (v.g - vd);
        let vds = p * (vd - vs);
        let vgb = p * (v.g - v.b);
        // Same for bulk junction diodes - polarities such that more `vsb`, `vdb` = more *reverse* bias.
        let vsb = p * (vs - v.b);
        let vdb = p * (vd - v.b);

        // Threshold & body effect calcs
        let von = if vsb > 0.0 {
            intp.vt0_t + model.gamma * ((intp.phi_t + vsb).sqrt() - intp.phi_t.sqrt())
        } else {
            intp.vt0_t // FIXME: body effect for Vsb < 0
        };
        let vov = vgs - von;
        let vdsat = vov.max(0.0);

        // Drain current & its g-derivatives
        // Default to cutoff values
        let mut ids = 0.0;
        let mut gm = 0.0;
        let mut gds = 0.0;
        let mut gmbs = 0.0;
        if vov > 0.0 {
            if vds >= vov {
                // Sat
                ids = intp.beta / 2.0 * vov.powi(2) * (1.0 + model.lambda * vds);
                gm = intp.beta * vov * (1.0 + model.lambda * vds);
                gds = model.lambda * intp.beta / 2.0 * vov.powi(2);
            } else {
                // Triode
                ids = intp.beta * (vov * vds - vds.powi(2) / 2.0) * (1.0 + model.lambda * vds);
                gm = intp.beta * vds * (1.0 + model.lambda * vds);
                gds = intp.beta * ((vov - vds) * (1.0 + model.lambda * vds) + model.lambda * ((vov * vds) - vds.powi(2) / 2.0));
            }
            gmbs = if intp.phi_t + vsb > 0.0 {
                gm * model.gamma / 2.0 / (intp.phi_t + vsb).sqrt()
            } else {
                0.0
            };
        }

        // Bulk Junction Diodes
        let Mos1InternalParams {
            vtherm,
            ref source_junc,
            ref drain_junc,
            ..
        } = intp;
        let (bs_junc, bd_junc) = if !reversed {
            (source_junc, drain_junc)
        } else {
            (drain_junc, source_junc)
        };
        // Source-Bulk
        let ibs = bs_junc.isat * ((-vsb / vtherm).exp() - 1.0);
        let gbs = (bs_junc.isat / vtherm) * (-vsb / vtherm).exp() + gmin;
        let ibs_rhs = ibs + vsb * gbs;
        // Drain-Bulk
        let ibd = bd_junc.isat * ((-vdb / vtherm).exp() - 1.0);
        let gbd = (bd_junc.isat / vtherm) * (-vdb / vtherm).exp() + gmin;
        let ibd_rhs = ibd + vdb * gbd;

        // Capacitance Calculations
        let cox = intp.cox;
        let cgs1: f64;
        let cgd1: f64;
        let cgb1: f64;
        if vov <= -intp.phi_t {
            cgb1 = cox / 2.0;
            cgs1 = 0.0;
            cgd1 = 0.0;
        } else if vov <= -intp.phi_t / 2.0 {
            cgb1 = -vov * cox / (2.0 * intp.phi_t);
            cgs1 = 0.0;
            cgd1 = 0.0;
        } else if vov <= 0.0 {
            cgb1 = -vov * cox / (2.0 * intp.phi_t);
            cgs1 = vov * cox / (1.5 * intp.phi_t) + cox / 3.0;
            cgd1 = 0.0;
        } else if vdsat <= vds {
            cgs1 = cox / 3.0;
            cgd1 = 0.0;
            cgb1 = 0.0;
        } else {
            let vddif = 2.0 * vdsat - vds;
            let vddif1 = vdsat - vds;
            let vddif2 = vddif * vddif;
            cgd1 = cox * (1.0 - vdsat * vdsat / vddif2) / 3.0;
            cgs1 = cox * (1.0 - vddif1 * vddif1 / vddif2) / 3.0;
            cgb1 = 0.0;
        }

        // Now start incorporating past history
        // FIXME: gotta sort out swaps in polarity between time-points
        // FIXME: this isnt quite right as we move from OP into first TRAN point. hacking that for now
        let cgs2 = if self.op.cgs == 0.0 {
            // This is the fake initial-time check to be cleaned
            cgs1
        } else if reversed == self.op.reversed {
            self.op.cgs
        } else {
            self.op.cgd
        };
        let cgs = cgs1 + cgs2 + intp.cgs_ov;
        let cgd = cgd1 + intp.cgd_ov + if reversed == self.op.reversed { self.op.cgd } else { self.op.cgs };
        let cgb = cgb1 + intp.cgb_ov + self.op.cgb;

        // Bulk Junction Caps
        let (_qbs, cbs) = bs_junc.qc(-vsb, model);
        let (_qbd, cbd) = bd_junc.qc(-vdb, model);

        // Transient Updates, Numerically Integrating each Cap
        let mut tr = Mos1TranState::default();
        if let AnalysisInfo::TRAN(_, state) = an {
            // Numerical integrations for cap currents and impedances
            {
                let dqgs = if reversed == self.op.reversed {
                    (vgs - self.op.vgs) * cgs
                } else {
                    (vgs - self.op.vgd) * cgs
                };
                let ip = if reversed == self.op.reversed {
                    self.op.tr.gs.i
                } else {
                    self.op.tr.gd.i
                };
                tr.gs = state.integq(dqgs, cgs, vgs, ip);
            }
            {
                let dqgd = if reversed == self.op.reversed {
                    (vgd - self.op.vgd) * cgd
                } else {
                    (vgd - self.op.vgs) * cgd
                };
                let ip = if reversed == self.op.reversed {
                    self.op.tr.gd.i
                } else {
                    self.op.tr.gs.i
                };
                tr.gs = state.integq(dqgd, cgd, vgd, ip);
            }
            {
                // Gate-Bulk Cap
                let dqgb = (vgb - self.op.vgb) * cgb;
                tr.gb = state.integq(dqgb, cgb, vgb, self.op.tr.gb.i);
            }
            {
                // Bulk Junction Caps
                let dqbs = if reversed == self.op.reversed {
                    (-vsb + self.op.vsb) * cbs
                } else {
                    (-vsb + self.op.vdb) * cbs
                };
                let dqbd = if reversed == self.op.reversed {
                    (-vdb + self.op.vdb) * cbd
                } else {
                    (-vdb + self.op.vsb) * cbd
                };
                let (isp, idp) = if reversed == self.op.reversed {
                    (self.op.tr.gs.i, self.op.tr.gd.i)
                } else {
                    (self.op.tr.gd.i, self.op.tr.gs.i)
                };
                tr.bs = state.integq(dqbs, cbs, -vsb, isp);
                tr.bd = state.integq(dqbd, cbd, -vdb, idp);
            }
        }
        let irhs = ids - gm * vgs - gds * vds;

        // Sort out which are the "reported" drain and source terminals (sr, dr)
        // FIXME: this also needs the "prime" vs "external" source & drains
        let (sr, sx, dr, dx) = if !reversed { (SP, S, DP, D) } else { (DP, D, SP, S) };
        // Include our terminal resistances
        let grd = intp.grd;
        let grs = intp.grs;
        // Collect up our matrix contributions
        let stamps = Stamps {
            g: vec![
                (self.matps[(dr, dr)], gds + grd + gbd + tr.gd.g),
                (self.matps[(sr, sr)], gm + gds + grs + gbs + gmbs + tr.gs.g),
                (self.matps[(dr, sr)], -gm - gds - gmbs),
                (self.matps[(sr, dr)], -gds),
                (self.matps[(dr, G)], gm - tr.gd.g),
                (self.matps[(sr, G)], -gm - tr.gs.g),
                (self.matps[(G, G)], (tr.gd.g + tr.gs.g + tr.gb.g)),
                (self.matps[(B, B)], (gbd + gbs + tr.gb.g)),
                (self.matps[(G, B)], -tr.gb.g),
                (self.matps[(G, dr)], -tr.gd.g),
                (self.matps[(G, sr)], -tr.gs.g),
                (self.matps[(B, G)], -tr.gb.g),
                (self.matps[(B, dr)], -gbd),
                (self.matps[(B, sr)], -gbs),
                (self.matps[(dr, B)], -gbd + gmbs),
                (self.matps[(sr, B)], -gbs - gmbs),
                (self.matps[(dx, dr)], -grd),
                (self.matps[(dr, dx)], -grd),
                (self.matps[(dx, dx)], grd),
                (self.matps[(sx, sr)], -grs),
                (self.matps[(sr, sx)], -grs),
                (self.matps[(sx, sx)], grs),
            ],
            b: vec![
                (self.ports[dr], p * (-irhs + ibd_rhs + tr.gd.rhs)),
                (self.ports[sr], p * (irhs + ibs_rhs + tr.gs.rhs)),
                (self.ports[G], -p * (tr.gs.rhs + tr.gb.rhs + tr.gd.rhs)),
                (self.ports[B], -p * (ibd_rhs + ibs_rhs - tr.gb.rhs)),
            ],
        };
        // Collect up an OpPoint for inter-iteration storage
        let guess = Mos1OpPoint {
            ids,
            vgs,
            vds,
            vgd,
            vgb,
            vdb,
            vsb,
            gm,
            gds,
            gmbs,
            gbs,
            gbd,
            reversed,
            cgs: cgs1,
            cgd: cgd1,
            cgb: cgb1,
            cbs,
            cbd,
            tr,
        };
        (guess, stamps)
    }
}
impl Component for Mos1 {
    fn create_matrix_elems<T: SpNum>(&mut self, mat: &mut Matrix<T>) {
        use Mos1Var::{B, D, DP, G, S, SP};
        for t1 in [G, D, S, B, DP, SP].iter() {
            for t2 in [G, D, S, B, DP, SP].iter() {
                self.matps[(*t1, *t2)] = make_matrix_elem(mat, self.ports[*t1], self.ports[*t2]);
            }
        }
    }
    fn commit(&mut self) {
        // Load our last guess as the new operating point
        self.op = self.guess.clone();
    }
    fn load(&mut self, vars: &Variables<f64>, an: &AnalysisInfo, opts: &Options) -> Stamps<f64> {
        let v = self.vs(vars); // Collect terminal voltages
        let (op, stamps) = self.op_stamp(v, an, opts); // Do most of our work here
        self.guess = op; // Save the calculated operating point
        stamps // And return our matrix stamps
    }
    fn load_ac(&mut self, _guess: &Variables<Complex<f64>>, an: &AnalysisInfo, _opts: &Options) -> Stamps<Complex<f64>> {
        let intp = &*self.intparams.read();

        // Grab the frequency-variable from our analysis
        let omega = match an {
            AnalysisInfo::AC(_opts, state) => state.omega,
            _ => panic!("Invalid AC AnalysisInfo"),
        };
        // Short-hand the conductances from our op-point.
        // (Rustc should be smart enough not to copy these.)
        let Mos1OpPoint { gm, gds, gmbs, gbs, gbd, .. } = self.op;
        // Cap admittances
        let gcgs = omega * self.op.cgs;
        let gcgd = omega * self.op.cgd;
        let gcgb = omega * self.op.cgb;
        let gcbs = omega * self.op.cbs;
        let gcbd = omega * self.op.cbd;

        // Sort out which are the "reported" drain and source terminals (sr, dr)
        use Mos1Var::{B, D, DP, G, S, SP};
        let (sr, sx, dr, dx) = if !self.op.reversed { (SP, S, DP, D) } else { (DP, D, SP, S) };

        // Include our terminal resistances
        // let Mos1InternalParams { intp.grs, intp.grd, .. } = intp;

        // And finally, send back our AC-matrix contributions
        return Stamps {
            g: vec![
                (self.matps[(dr, dr)], Complex::new(gds + intp.grd + gbd, gcgd)),
                (self.matps[(sr, sr)], Complex::new(gm + gds + intp.grs + gbs + gmbs, gcgs)),
                (self.matps[(dr, sr)], Complex::new(-gm - gds - gmbs, 0.0)),
                (self.matps[(sr, dr)], Complex::new(-gds, 0.0)),
                (self.matps[(dr, G)], Complex::new(gm, -gcgd)),
                (self.matps[(sr, G)], Complex::new(-gm, -gcgs)),
                (self.matps[(G, G)], Complex::new(0.0, gcgd + gcgs + gcgb)),
                (self.matps[(B, B)], Complex::new(gbd + gbs, gcgb)),
                (self.matps[(G, B)], Complex::new(0.0, -gcgb)),
                (self.matps[(G, dr)], Complex::new(0.0, -gcgd)),
                (self.matps[(G, sr)], Complex::new(0.0, -gcgs)),
                (self.matps[(B, G)], Complex::new(0.0, -gcgb)),
                (self.matps[(G, dr)], Complex::new(0.0, -gcgd)),
                (self.matps[(B, dr)], Complex::new(-gbd, 0.0)),
                (self.matps[(B, sr)], Complex::new(-gbs, 0.0)),
                (self.matps[(dr, B)], Complex::new(-gbd + gmbs, 0.0)),
                (self.matps[(sr, B)], Complex::new(-gbs - gmbs, 0.0)),
                (self.matps[(dx, dr)], Complex::new(-intp.grd, 0.0)),
                (self.matps[(dr, dx)], Complex::new(-intp.grd, 0.0)),
                (self.matps[(dx, dx)], Complex::new(intp.grd, 0.0)),
                (self.matps[(sx, sr)], Complex::new(-intp.grs, 0.0)),
                (self.matps[(sr, sx)], Complex::new(-intp.grs, 0.0)),
                (self.matps[(sx, sx)], Complex::new(intp.grs, 0.0)),
            ],
            b: vec![],
        };
    }
}

use crate::defs::{CacheEntry, ModelInstanceCache};

///
/// # Mos1 Model and Instance-Param Definitions Depot
///
pub(crate) type Mos1Defs = ModelInstanceCache<Mos1Model, Mos1InstanceParams, Mos1CacheEntry>;

#[derive(Default)]
pub(crate) struct Mos1CacheEntry {
    pub(crate) model: DefPtr<Mos1Model>,
    pub(crate) inst: DefPtr<Mos1InstanceParams>,
    pub(crate) intp: DefPtr<Mos1InternalParams>,
}
impl Clone for Mos1CacheEntry {
    fn clone(&self) -> Self {
        Self {
            model: DefPtr::clone(&self.model),
            inst: DefPtr::clone(&self.inst),
            intp: DefPtr::clone(&self.intp),
        }
    }
}
impl CacheEntry for Mos1CacheEntry {
    type Model = Mos1Model;
    type Instance = Mos1InstanceParams;

    fn new(model: &DefPtr<Self::Model>, inst: &DefPtr<Self::Instance>, opts: &Options) -> Self {
        let intp = Mos1InternalParams::derive(&*model.read(), &*inst.read(), opts);
        Self {
            intp: DefPtr::new(intp),
            inst: DefPtr::clone(inst),
            model: DefPtr::clone(model),
        }
    }
}

/// Mos Level-Zero Instance Parameters
pub(crate) struct Mos0Params {
    mos_type: MosType,
    vth: f64,
    beta: f64,
    lam: f64,
}
impl Default for Mos0Params {
    fn default() -> Self {
        Mos0Params {
            mos_type: MosType::NMOS,
            vth: 0.25,
            beta: 50e-3,
            lam: 3e-3,
        }
    }
}

/// Mos "Level Zero" Simplified Solver
pub struct Mos0 {
    params: Mos0Params,
    ports: MosPorts<Option<VarIndex>>,
    matps: Mos0MatrixPointers,
}
impl Mos0 {
    pub(crate) fn new(ports: MosPorts<Option<VarIndex>>, mos_type: MosType) -> Self {
        Mos0 {
            params: Mos0Params {
                mos_type: mos_type,
                ..Mos0Params::default()
            },
            ports,
            matps: Mos0MatrixPointers([[None; 4]; 4]),
        }
    }
}
impl Component for Mos0 {
    fn create_matrix_elems<T: SpNum>(&mut self, mat: &mut Matrix<T>) {
        use MosTerm::{D, G, S};
        let matps = [(D, D), (S, S), (D, S), (S, D), (D, G), (S, G)];
        for (t1, t2) in matps.iter() {
            self.matps[(*t1, *t2)] = make_matrix_elem(mat, self.ports[*t1], self.ports[*t2]);
        }
    }
    fn load(&mut self, guess: &Variables<f64>, _an: &AnalysisInfo, opts: &Options) -> Stamps<f64> {
        use MosTerm::{D, G, S};
        let gmin = opts.gmin;

        let vg = guess.get(self.ports[G]);
        let vd = guess.get(self.ports[D]);
        let vs = guess.get(self.ports[S]);

        let p = self.params.mos_type.p();
        let vds1 = p * (vd - vs);
        let reversed = vds1 < 0.0;
        let vgs = if reversed { p * (vg - vd) } else { p * (vg - vs) };
        let vds = if reversed { -vds1 } else { vds1 };
        let vov = vgs - self.params.vth;

        // Cutoff conditions
        let mut ids = 0.0;
        let mut gm = 0.0;
        let mut gds = 0.0;
        if vov > 0.0 {
            let lam = self.params.lam;
            let beta = self.params.beta;
            if vds >= vov {
                // Saturation
                ids = beta / 2.0 * vov.powi(2) * (1.0 + lam * vds);
                gm = beta * vov * (1.0 + lam * vds);
                gds = lam * beta / 2.0 * vov.powi(2);
            } else {
                // Triode
                ids = beta * (vov * vds - vds.powi(2) / 2.0) * (1.0 + lam * vds);
                gm = beta * vds * (1.0 + lam * vds);
                gds = beta * ((vov - vds) * (1.0 + lam * vds) + lam * ((vov * vds) - vds.powi(2) / 2.0));
            }
        }
        // Sort out which are the "reported" drain and source terminals (sr, dr)
        let (sr, dr) = if !reversed { (S, D) } else { (D, S) };
        let irhs = ids - gm * vgs - gds * vds;
        return Stamps {
            g: vec![
                (self.matps[(dr, dr)], gds + gmin),
                (self.matps[(sr, sr)], (gm + gds + gmin)),
                (self.matps[(dr, sr)], -(gm + gds + gmin)),
                (self.matps[(sr, dr)], -gds - gmin),
                (self.matps[(dr, G)], gm),
                (self.matps[(sr, G)], -gm),
            ],
            b: vec![(self.ports[dr], -p * irhs), (self.ports[sr], p * irhs)],
        };
    }
}
#[derive(Default)]
struct Mos0MatrixPointers([[Option<Eindex>; 4]; 4]);
impl Index<(MosTerm, MosTerm)> for Mos0MatrixPointers {
    type Output = Option<Eindex>;
    fn index(&self, ts: (MosTerm, MosTerm)) -> &Option<Eindex> {
        &self.0[ts.0 as usize][ts.1 as usize]
    }
}
impl IndexMut<(MosTerm, MosTerm)> for Mos0MatrixPointers {
    fn index_mut(&mut self, ts: (MosTerm, MosTerm)) -> &mut Self::Output {
        &mut self.0[ts.0 as usize][ts.1 as usize]
    }
}
