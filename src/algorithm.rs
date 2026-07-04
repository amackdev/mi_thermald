use std::sync::atomic::Ordering;

use crate::types::*;

pub fn evaluate_virtual_sensor(sensors: &[Sensor], vsn_idx: usize) -> i32 {
    let vsn = &sensors[vsn_idx];
    // FIX BUG-001: Cache weight_sum to prevent TOCTOU race condition
    let weight_sum = vsn.weight_sum;
    if vsn.inputs.is_empty() || weight_sum == 0 {
        return -1;
    }

    let mut acc: i64 = 0;
    for (i, &input_idx) in vsn.inputs.iter().enumerate() {
        let v = sensors[input_idx].last_temp_mc.load(Ordering::Relaxed) as i64;
        let w = if i < vsn.weights.len() { vsn.weights[i] as i64 } else { 0 };
        acc += v * w;
    }
    let result = (acc / weight_sum as i64) as i32 + vsn.compensation;
    sensors[vsn_idx].last_temp_mc.store(result, Ordering::Relaxed);
    result
}

pub fn algo_sic(inst: &Instance, temp_mc: i32, state: &mut SicState) -> i32 {
    let t = &inst.threshold;
    if t.n_levels() == 0 {
        return 0;
    }

    let mut seg = 0;
    // FIX BUG-008: Add bounds checking to prevent OOB access
    for i in 0..t.n_levels().min(t.trig.len()) {
        if temp_mc >= t.trig[i] {
            seg = i;
        }
    }

    let target = if seg < t.target.len() { t.target[seg] } else { 0 };
    let ek = target - temp_mc;

    let output_now = if !state.initialized {
        state.ek_1 = ek;
        state.ek_2 = ek;
        state.initialized = true;
        state.initial_value
    } else {
        inst.current_value
    };

    let ks_val = if seg < t.ks.len() { t.ks[seg] as i64 } else { 0 };
    let ki_val = if seg < t.ki.len() { t.ki[seg] as i64 } else { 0 };
    let kc_val = if seg < t.kc.len() { t.kc[seg] as i64 } else { 0 };

    // Original convert_target_to_output formula:
    // delta = ks*(Ek - 2*Ek_1 + Ek_2)/1000 + kc*(Ek - Ek_1)/1000 + ki*Ek/1000
    let ek_1 = state.ek_1 as i64;
    let ek_2 = state.ek_2 as i64;
    let ek_i = ek as i64;

    let delta_num = ks_val * (ek_i - 2 * ek_1 + ek_2)
                  + kc_val * (ek_i - ek_1)
                  + ki_val * ek_i;
    let delta = (delta_num as f64 / 1000.0).round() as i64;

    state.ek_2 = state.ek_1;
    state.ek_1 = ek;

    let mut output = (output_now as i64) + delta;

    let max_o = if seg < t.max_out.len() { (t.max_out[seg] as i64) * 1000 } else { i32::MAX as i64 };
    let min_o = if seg < t.min_out.len() { (t.min_out[seg] as i64) * 1000 } else { 0 };
    output = output.clamp(min_o, max_o);

    output as i32
}

pub fn evaluate_instance(
    inst: &mut Instance,
    temp_mc: i32,
    sic_state: &mut SicState,
) -> i32 {
    let n_levels = inst.threshold.n_levels() as i32;
    if n_levels == 0 {
        return 0;
    }

    let raw_level = if inst.algo == AlgoType::Sic {
        let mut lvl = 0i32;
        // FIX BUG-008: Add bounds checking
        for i in 0..inst.threshold.n_levels().min(inst.threshold.trig.len()) {
            if temp_mc >= inst.threshold.trig[i] {
                // FIX BUG-003: Use saturating conversion to prevent overflow
                lvl = (i + 1).min(i32::MAX as usize) as i32;
            }
        }
        inst.current_value = algo_sic(inst, temp_mc, sic_state);
        lvl
    } else {
        let mut lvl = 0i32;
        // FIX BUG-008: Add bounds checking
        for i in (0..inst.threshold.n_levels().min(inst.threshold.trig.len())).rev() {
            let crossed = if inst.reverse {
                temp_mc <= inst.threshold.trig[i]
            } else {
                temp_mc >= inst.threshold.trig[i]
            };
            if crossed {
                // FIX BUG-003: Use saturating conversion to prevent overflow
                lvl = (i + 1).min(i32::MAX as usize) as i32;
                break;
            }
        }
        lvl
    };

    let raw_level = raw_level.max(0).min(n_levels);

    let new_level = if raw_level < inst.current_level {
        let clr_idx = (inst.current_level - 1) as usize;
        if clr_idx < inst.threshold.clr.len() {
            let cleared = if inst.reverse {
                temp_mc >= inst.threshold.clr[clr_idx]
            } else {
                temp_mc <= inst.threshold.clr[clr_idx]
            };
            if cleared {
                raw_level
            } else {
                inst.current_level
            }
        } else {
            raw_level
        }
    } else {
        raw_level
    };

    if new_level != inst.current_level {
        log_info!(
            "instance {}: temp={} mC, level={} -> {}",
            inst.name, temp_mc, inst.current_level, new_level
        );
        if new_level > 0 && inst.sensor_idx.is_some() {
            let _si = inst.sensor_idx.unwrap();
            log_info!("  {} trig={:?} clr={:?} reverse={}",
                inst.name, inst.threshold.trig, inst.threshold.clr, inst.reverse);
        }
        inst.current_level = new_level;
    }
    new_level
}
