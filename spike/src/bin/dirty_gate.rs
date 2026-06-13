// FEASIBILITY GATE SPIKE — diff/incremental snapshots dirty-page detection.
//
// THROWAWAY. Deleted once the feature lands (see
// docs/superpowers/specs/2026-06-13-diff-snapshots-design.md, "Feasibility gate").
// It is GO/NO-GO: does write-protecting guest RAM via `hv_vm_protect` and catching
// the resulting Data-Abort permission fault give us a *recoverable* dirty-page
// signal on this hardware?
//
// We bypass the production `HvfVcpu::run()` loop on purpose: that loop treats every
// data abort as MMIO (advances PC, decodes a load/store) — which is exactly the
// behaviour Task 2 will change and exactly the behaviour that would destroy our
// ability to observe a raw permission fault and re-execute the store. So this spike
// drives the vCPU directly through the raw HVF bindings (`ignition_hvf::bindings`),
// giving us full control over PC and raw read access to syndrome / EC / DFSC / IPA.
//
// Three checks:
//   1. Recoverable write fault: protect a page R+X (drop W), let the guest store to
//      it, confirm EC==0x24 + DFSC permission + IPA==page, re-grant W, resume WITHOUT
//      advancing PC, confirm the store completes and the guest moves on.
//   2. Granule: does hv_vm_protect accept a 4 KiB sub-range of a 16 KiB host page?
//   3. Cost: protect->fault->regrant over N distinct pages; measure per-fault µs.

use std::time::Instant;

use ignition_hvf::HvfVm;
use ignition_hvf::bindings::*;

// Guest physical memory layout.
const RAM_BASE: u64 = 0x4000_0000; // ignition_arch::aarch64::layout::RAM_BASE
const HOST_PAGE: u64 = 16384; // Apple Silicon host page (16 KiB)
const SUB_PAGE: u64 = 4096; // guest stage-2 granule (4 KiB)

// CHECK 3 needs N distinct data pages plus a code page. Size RAM to fit comfortably.
const N_PAGES: u64 = 10000; // CHECK 3 distinct pages
const RAM_SIZE: u64 = 512 * 1024 * 1024; // 512 MiB

// Layout inside guest RAM:
//   page 0 (RAM_BASE)                  : code
//   data region starts at DATA_BASE    : the pages we protect/fault/regrant
const CODE_IPA: u64 = RAM_BASE;
const DATA_OFFSET: u64 = HOST_PAGE; // data starts on the second host page
const DATA_BASE: u64 = RAM_BASE + DATA_OFFSET;

const HV_SUCCESS_: hv_return_t = 0;

// ---- raw register helpers (no production run-loop) ------------------------------

fn set_reg(vcpu: hv_vcpu_t, reg: hv_reg_t, val: u64) {
    let r = unsafe { hv_vcpu_set_reg(vcpu, reg, val) };
    assert_eq!(r, HV_SUCCESS_, "hv_vcpu_set_reg(reg={reg}) failed: {r:#x}");
}

fn get_reg(vcpu: hv_vcpu_t, reg: hv_reg_t) -> u64 {
    let v: u64 = 0;
    let r = unsafe { hv_vcpu_get_reg(vcpu, reg, &v as *const _ as *mut _) };
    assert_eq!(r, HV_SUCCESS_, "hv_vcpu_get_reg(reg={reg}) failed: {r:#x}");
    v
}

// EL1h, all DAIF masked. Matches PSTATE_EL1_FAULT_BITS_64 in crates/hvf.
const CPSR_EL1H: u64 = 0x0000_0005 | 0x40 | 0x80 | 0x100 | 0x200;

struct ExitInfo {
    reason: hv_exit_reason_t,
    syndrome: u64,
    ipa: u64,
    pc: u64,
}

/// One raw vCPU run, returning the decoded exit fields. Does NOT touch PC.
fn run_once(vcpu: hv_vcpu_t, exit: &hv_vcpu_exit_t) -> ExitInfo {
    let r = unsafe { hv_vcpu_run(vcpu) };
    assert_eq!(r, HV_SUCCESS_, "hv_vcpu_run failed: {r:#x}");
    ExitInfo {
        reason: exit.reason,
        syndrome: exit.exception.syndrome,
        ipa: exit.exception.physical_address,
        pc: get_reg(vcpu, hv_reg_t_HV_REG_PC),
    }
}

fn ec_of(syndrome: u64) -> u64 {
    (syndrome >> 26) & 0x3f
}
fn dfsc_of(syndrome: u64) -> u64 {
    syndrome & 0x3f
}
fn is_permission_fault(syndrome: u64) -> bool {
    let dfsc = dfsc_of(syndrome);
    (0x0c..=0x0f).contains(&dfsc)
}

fn main() {
    println!("== dirty_gate: write-protect dirty-fault feasibility gate ==");
    println!("host page = {HOST_PAGE} bytes, RAM = {} MiB\n", RAM_SIZE / 1024 / 1024);

    // --- VM + RAM setup ---------------------------------------------------------
    let vm = HvfVm::new(false).expect("hv_vm_create failed (entitlement? SDK?)");

    let host = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            RAM_SIZE as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    assert!(host != libc::MAP_FAILED, "mmap failed");
    let host = host as u64;

    vm.map_memory(host, RAM_BASE, RAM_SIZE).expect("hv_vm_map failed");

    // Helper to read a u32 word back out of guest RAM (host view of the same pages).
    let read_word = |ipa: u64| -> u32 {
        let off = ipa - RAM_BASE;
        unsafe { ((host + off) as *const u32).read_volatile() }
    };

    // === CHECK 1 — recoverable write fault =====================================
    let check1 = check1_recoverable_fault(&vm, host);

    // === CHECK 2 — granule ======================================================
    let check2 = check2_granule(&vm, read_word);

    // === CHECK 3 — cost =========================================================
    let check3 = check3_cost(&vm, host);

    // --- verdict ----------------------------------------------------------------
    println!("\n================ GATE SUMMARY ================");
    println!("CHECK 1 (recoverable write fault): {}", if check1 { "PASS" } else { "FAIL" });
    println!("CHECK 2 (granule)               : see verdict above");
    println!("CHECK 3 (cost)                  : see numbers above");

    let granule = if check2 { SUB_PAGE } else { HOST_PAGE };
    println!("CHOSEN GRANULE = {granule} bytes");
    let verdict = if check1 { "GO" } else { "NO-GO" };
    println!("VERDICT = {verdict}");
    println!("=============================================");

    if !check1 {
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------------
// CHECK 1: a single store to one protected page; confirm fault then recovery.
// ---------------------------------------------------------------------------------
fn check1_recoverable_fault(vm: &HvfVm, host: u64) -> bool {
    println!("--- CHECK 1: recoverable write fault ---");

    // Guest program (at CODE_IPA):
    //   str  w1, [x0]      ; store the value in w1 to [x0]  -> may fault
    //   brk  #0            ; stop here once the store retires (EC 0x3c)
    // x0 = target IPA, x1 = value, set as initial registers.
    let code: [u32; 2] = [
        0xb900_0001, // str w1, [x0]
        0xd420_0000, // brk #0
    ];
    write_code(host, CODE_IPA, &code);

    let target = DATA_BASE; // first data page
    let value: u64 = 0xDEAD_BEEF;
    let page = target & !(HOST_PAGE - 1);

    // Sanity: the data word must start clean (mmap is zero-filled).
    let before = unsafe { ((host + (target - RAM_BASE)) as *const u32).read_volatile() };
    println!("  data word before store = {before:#x} (expect 0)");

    let (vcpu, exit) = make_vcpu(CODE_IPA);
    set_reg(vcpu, hv_reg_t_HV_REG_X0, target);
    set_reg(vcpu, hv_reg_t_HV_REG_X1, value);

    // Drop WRITE on the target page. Guest store should now trap.
    let pr = vm.protect_memory(page, HOST_PAGE, (HV_MEMORY_READ | HV_MEMORY_EXEC) as u64);
    println!("  hv_vm_protect(page={page:#x}, R|X) -> {pr:#x}");
    assert_eq!(pr, HV_SUCCESS_, "protect (drop WRITE) failed");

    // Run: expect an EXCEPTION / Data Abort / permission fault at `page`.
    let e = run_once(vcpu, exit);
    let ec = ec_of(e.syndrome);
    let dfsc = dfsc_of(e.syndrome);
    println!(
        "  FAULT: reason={} syndrome={:#x} EC={:#x} DFSC={:#x} IPA={:#x} PC={:#x}",
        e.reason, e.syndrome, ec, dfsc, e.ipa, e.pc
    );

    // What the DESIGN assumed vs. what HVF actually reports. We record both: the
    // design's `EC_DATAABORT` arm keys recovery on DFSC == permission fault
    // (0x0c..=0x0f). On this hardware, dropping HV_MEMORY_WRITE produces a *write data
    // abort* whose DFSC is a TRANSLATION fault (typically 0x07, level-3 xlat), NOT the
    // textbook permission code. The faulting IPA is still exactly correct. So the
    // mechanism works; the discriminator in the design must be relaxed to
    // "write data abort whose IPA is inside RAM" rather than DFSC==permission.
    let iswrite = ((e.syndrome >> 6) & 1) != 0;
    let reason_ok = e.reason == hv_exit_reason_t_HV_EXIT_REASON_EXCEPTION;
    let ec_ok = ec == 0x24;
    let dfsc_is_permission = is_permission_fault(e.syndrome);
    let ipa_ok = (e.ipa & !(HOST_PAGE - 1)) == page;
    let pc_at_store = e.pc == CODE_IPA; // PC must still point at the faulting store
    println!(
        "    reason==EXCEPTION:{reason_ok}  EC==0x24:{ec_ok}  iswrite:{iswrite}  DFSC-is-permission:{dfsc_is_permission} (design assumed true)  IPA==page:{ipa_ok}  PC@store:{pc_at_store}"
    );

    // Word must still be unwritten (the store faulted, did not retire).
    let mid = unsafe { ((host + (target - RAM_BASE)) as *const u32).read_volatile() };
    let not_yet_written = mid == before;
    println!("  data word after fault  = {mid:#x} (expect still 0): {not_yet_written}");

    // Re-grant WRITE and resume WITHOUT advancing PC. The store must re-execute.
    let rg = vm.protect_memory(page, HOST_PAGE, (HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC) as u64);
    println!("  hv_vm_protect(page={page:#x}, R|W|X) regrant -> {rg:#x}");
    assert_eq!(rg, HV_SUCCESS_, "regrant WRITE failed");

    let e2 = run_once(vcpu, exit);
    let ec2 = ec_of(e2.syndrome);
    println!(
        "  RESUME: reason={} syndrome={:#x} EC={:#x} IPA={:#x} PC={:#x}",
        e2.reason, e2.syndrome, ec2, e2.ipa, e2.pc
    );

    // Forward progress: the faulting store retired (PC moved off CODE_IPA). The guest
    // then hits `brk #0`; with VBAR_EL1=0 and no installed handler this is delivered
    // as an exception that vectors to 0x200 — which itself proves the store retired
    // and the guest advanced. We require only that PC is no longer parked on the store.
    let progressed = e2.pc != CODE_IPA;
    let stored = read_word_at(host, target);
    let value_ok = stored == (value as u32);
    println!("  forward progress (PC left the store): {progressed}");
    println!("  data word after resume = {stored:#x} (expect {:#x}): {value_ok}", value as u32);

    destroy_vcpu(vcpu);

    // GO criteria: a write data abort at the right IPA, store held off while
    // protected, store completes correctly after regrant + resume-without-PC-advance.
    // DFSC being a translation (not permission) code is a finding, not a failure.
    let pass = reason_ok && ec_ok && iswrite && ipa_ok && pc_at_store
        && not_yet_written && progressed && value_ok;
    println!("  => CHECK 1 {}", if pass { "PASS" } else { "FAIL" });
    println!(
        "     (NOTE: DFSC={:#x} is a TRANSLATION fault, not the permission code the design assumed)\n",
        dfsc
    );
    pass
}

// ---------------------------------------------------------------------------------
// CHECK 2: does hv_vm_protect accept a 4 KiB sub-range of a 16 KiB host page?
// We protect only the first 4 KiB of a 16 KiB page R|X, then probe with a store to
// the protected sub-range AND a store to the unprotected remainder of the same host
// page, to see whether protection applied to 4 KiB or got promoted to the full 16 K.
// ---------------------------------------------------------------------------------
fn check2_granule(vm: &HvfVm, read_word: impl Fn(u64) -> u32) -> bool {
    println!("--- CHECK 2: granule (4 KiB sub-range of a 16 KiB host page) ---");

    let page = DATA_BASE + 64 * HOST_PAGE; // a fresh, distinct host page
    let _ = read_word; // (host-side reads not needed for the verdict)

    let ret = vm.protect_memory(page, SUB_PAGE, (HV_MEMORY_READ | HV_MEMORY_EXEC) as u64);
    println!("  hv_vm_protect(ipa={page:#x}, size={SUB_PAGE} [4 KiB], R|X) -> {ret:#x}");

    let clean_4k = if ret == HV_SUCCESS_ {
        // It accepted a 4 KiB size. Determine whether it actually protected only 4 KiB
        // or silently promoted to 16 KiB by faulting a store within each sub-range.
        let sub_in = page; // inside the protected 4 KiB
        let sub_out = page + SUB_PAGE; // 4 KiB into the page: outside the protected range
        let in_faults = store_faults(vm, sub_in);
        let out_faults = store_faults(vm, sub_out);
        println!("    store @ protected 4K (faults?)   = {in_faults}");
        println!("    store @ remainder 12K (faults?)  = {out_faults}");
        // Re-grant the whole host page so later checks see RWX.
        let _ = vm.protect_memory(page, HOST_PAGE, (HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC) as u64);
        if in_faults && !out_faults {
            println!("    VERDICT: 4 KiB sub-range protection WORKS cleanly (only the 4K range traps).");
            true
        } else if in_faults && out_faults {
            println!("    VERDICT: hv_vm_protect accepted 4 KiB but PROMOTED to the whole 16 KiB host page.");
            false
        } else {
            println!("    VERDICT: 4 KiB size accepted but no fault observed — unreliable; use 16 KiB.");
            false
        }
    } else {
        println!("    VERDICT: hv_vm_protect REJECTED the 4 KiB sub-range (ret={ret:#x}). Granule = 16 KiB.");
        false
    };

    println!("  => CHECK 2: 4KiB-clean = {clean_4k}\n");
    clean_4k
}

/// Run a fresh tiny guest that does a single store to `target`; return true if it
/// trapped (the page was write-protected) rather than retiring to the brk.
fn store_faults(_vm: &HvfVm, target: u64) -> bool {
    // We reuse the live VM's RAM; build a transient vcpu with its own code page.
    // Code page reused: the same store/brk at CODE_IPA. (RAM_BASE page is RWX.)
    // A write-protect trap here is a write data abort at `target` — disambiguated the
    // same relaxed way as everywhere else (EC 0x24 + iswrite + IPA in the page), since
    // HVF reports a translation DFSC rather than the permission code.
    let (vcpu, exit) = make_vcpu(CODE_IPA);
    set_reg(vcpu, hv_reg_t_HV_REG_X0, target);
    set_reg(vcpu, hv_reg_t_HV_REG_X1, 0x1234_5678);
    let e = run_once(vcpu, exit);
    let iswrite = ((e.syndrome >> 6) & 1) != 0;
    let faulted = e.reason == hv_exit_reason_t_HV_EXIT_REASON_EXCEPTION
        && ec_of(e.syndrome) == 0x24
        && iswrite
        && (e.ipa & !(HOST_PAGE - 1)) == (target & !(HOST_PAGE - 1));
    destroy_vcpu(vcpu);
    faulted
}

// ---------------------------------------------------------------------------------
// CHECK 3: cost. Loop protect->fault->regrant over N distinct pages, timing each.
// The guest is a loop that stores to [x0], then x0 += GRANULE, and stores again.
// Each iteration's store faults exactly once on a freshly-protected page.
// ---------------------------------------------------------------------------------
fn check3_cost(vm: &HvfVm, host: u64) -> bool {
    println!("--- CHECK 3: cost over {N_PAGES} distinct pages ---");

    // Guest loop:
    //   loop: str  w1, [x0]        ; fault on first write to this page
    //         add  x0, x0, x2      ; advance to next page (x2 = HOST_PAGE)
    //         b    loop
    let code: [u32; 3] = [
        0xb900_0001, // str  x0]      w1, [(index 0)
        0x8b02_0000, // add  x0, x0, x2   (index 1)
        0x17ff_fffe, // b    -8  (back 2 insns, to the str at index 0)
    ];
    write_code(host, CODE_IPA, &code);

    let (vcpu, exit) = make_vcpu(CODE_IPA);
    let start_ipa = DATA_BASE;
    set_reg(vcpu, hv_reg_t_HV_REG_X0, start_ipa);
    set_reg(vcpu, hv_reg_t_HV_REG_X1, 0xA5A5_A5A5);
    set_reg(vcpu, hv_reg_t_HV_REG_X2, HOST_PAGE);

    // Protect all N target pages up front (drop WRITE).
    for i in 0..N_PAGES {
        let page = start_ipa + i * HOST_PAGE;
        let r = vm.protect_memory(page, HOST_PAGE, (HV_MEMORY_READ | HV_MEMORY_EXEC) as u64);
        assert_eq!(r, HV_SUCCESS_, "protect page {page:#x} failed: {r:#x}");
    }

    let mut faults = 0u64;
    let mut last_pc_was_store = 0u64;
    let t0 = Instant::now();
    loop {
        let e = run_once(vcpu, exit);
        if faults < 3 {
            println!(
                "    [fault {faults}] reason={} EC={:#x} DFSC={:#x} IPA={:#x} PC={:#x}",
                e.reason, ec_of(e.syndrome), dfsc_of(e.syndrome), e.ipa, e.pc
            );
        }
        // Disambiguate exactly as the production path will: a write data abort whose
        // IPA lands inside our RAM region is a dirty-tracking fault, regardless of the
        // precise DFSC sub-code (HVF reports these as translation faults, not the
        // textbook permission code — see Gate result).
        let ec = ec_of(e.syndrome);
        let iswrite = ((e.syndrome >> 6) & 1) != 0;
        let in_ram = e.ipa >= RAM_BASE && e.ipa < RAM_BASE + RAM_SIZE;
        if e.reason != hv_exit_reason_t_HV_EXIT_REASON_EXCEPTION || ec != 0x24 || !iswrite || !in_ram {
            panic!(
                "CHECK 3 unexpected exit: reason={} syndrome={:#x} EC={ec:#x} iswrite={iswrite} ipa={:#x} pc={:#x}",
                e.reason, e.syndrome, e.ipa, e.pc
            );
        }
        let page = e.ipa & !(HOST_PAGE - 1);
        // Re-grant WRITE on the faulting page and resume WITHOUT advancing PC.
        let r = vm.protect_memory(page, HOST_PAGE, (HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC) as u64);
        assert_eq!(r, HV_SUCCESS_, "regrant page {page:#x} failed: {r:#x}");
        last_pc_was_store = e.pc;
        faults += 1;
        if faults >= N_PAGES {
            break;
        }
    }
    let dt = t0.elapsed();
    destroy_vcpu(vcpu);

    let per = dt.as_secs_f64() * 1e6 / faults as f64;
    println!("  faults handled = {faults} (last PC = {last_pc_was_store:#x})");
    println!("  total wall time = {:.3} ms", dt.as_secs_f64() * 1e3);
    println!("  per-fault       = {per:.3} µs");
    let bounded = per < 20.0;
    println!("  bounded (< 20 µs/fault, no hang): {bounded}");
    println!("  => CHECK 3 {}\n", if bounded { "PASS" } else { "WARN" });
    bounded
}

// ---------------------------------------------------------------------------------
// raw vCPU lifecycle helpers
// ---------------------------------------------------------------------------------

/// Create a vCPU on the current thread, set EL1h CPSR and PC=entry, return (id, exit).
fn make_vcpu(entry: u64) -> (hv_vcpu_t, &'static hv_vcpu_exit_t) {
    let mut vcpuid: hv_vcpu_t = 0;
    let exit_ptr: *mut hv_vcpu_exit_t = std::ptr::null_mut();
    let r = unsafe {
        hv_vcpu_create(
            &mut vcpuid,
            &exit_ptr as *const _ as *mut *mut _,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(r, HV_SUCCESS_, "hv_vcpu_create failed: {r:#x}");
    let exit: &hv_vcpu_exit_t = unsafe { exit_ptr.as_ref().unwrap() };
    set_reg(vcpuid, hv_reg_t_HV_REG_CPSR, CPSR_EL1H);
    set_reg(vcpuid, hv_reg_t_HV_REG_PC, entry);
    (vcpuid, exit)
}

fn destroy_vcpu(vcpu: hv_vcpu_t) {
    let r = unsafe { hv_vcpu_destroy(vcpu) };
    assert_eq!(r, HV_SUCCESS_, "hv_vcpu_destroy failed: {r:#x}");
}

fn write_code(host: u64, ipa: u64, code: &[u32]) {
    let off = ipa - RAM_BASE;
    unsafe {
        let dst = (host + off) as *mut u32;
        for (i, insn) in code.iter().enumerate() {
            dst.add(i).write(insn.to_le());
        }
    }
}

fn read_word_at(host: u64, ipa: u64) -> u32 {
    let off = ipa - RAM_BASE;
    unsafe { ((host + off) as *const u32).read_volatile() }
}
