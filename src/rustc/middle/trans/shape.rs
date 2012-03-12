// A "shape" is a compact encoding of a type that is used by interpreted glue.
// This substitutes for the runtime tags used by e.g. MLs.

import lib::llvm::llvm;
import lib::llvm::{True, False, ModuleRef, TypeRef, ValueRef};
import driver::session;
import driver::session::session;
import trans::base;
import middle::trans::common::*;
import back::abi;
import middle::ty;
import middle::ty::field;
import syntax::ast;
import syntax::ast_util::dummy_sp;
import syntax::util::interner;
import util::common;
import trans::build::{Load, Store, Add, GEPi};
import syntax::codemap::span;

import std::map::hashmap;

import ty_ctxt = middle::ty::ctxt;

type res_info = {did: ast::def_id, tps: [ty::t]};

type ctxt =
    {mutable next_tag_id: u16,
     pad: u16,
     tag_id_to_index: hashmap<ast::def_id, u16>,
     mutable tag_order: [ast::def_id],
     resources: interner::interner<res_info>,
     llshapetablesty: TypeRef,
     llshapetables: ValueRef};

const shape_u8: u8 = 0u8;
const shape_u16: u8 = 1u8;
const shape_u32: u8 = 2u8;
const shape_u64: u8 = 3u8;
const shape_i8: u8 = 4u8;
const shape_i16: u8 = 5u8;
const shape_i32: u8 = 6u8;
const shape_i64: u8 = 7u8;
const shape_f32: u8 = 8u8;
const shape_f64: u8 = 9u8;
const shape_box: u8 = 10u8;
const shape_vec: u8 = 11u8;
const shape_enum: u8 = 12u8;
const shape_box_old: u8 = 13u8; // deprecated, remove after snapshot
const shape_struct: u8 = 17u8;
const shape_box_fn: u8 = 18u8;
const shape_UNUSED: u8 = 19u8;
const shape_res: u8 = 20u8;
const shape_var: u8 = 21u8;
const shape_uniq: u8 = 22u8;
const shape_opaque_closure_ptr: u8 = 23u8; // the closure itself.
const shape_uniq_fn: u8 = 25u8;
const shape_stack_fn: u8 = 26u8;
const shape_bare_fn: u8 = 27u8;
const shape_tydesc: u8 = 28u8;
const shape_send_tydesc: u8 = 29u8;
const shape_class: u8 = 30u8;
const shape_rptr: u8 = 31u8;

fn hash_res_info(ri: res_info) -> uint {
    let h = 5381u;
    h *= 33u;
    h += ri.did.crate as uint;
    h *= 33u;
    h += ri.did.node as uint;
    for t in ri.tps {
        h *= 33u;
        h += ty::type_id(t);
    }
    ret h;
}

fn mk_global(ccx: @crate_ctxt, name: str, llval: ValueRef, internal: bool) ->
   ValueRef {
    let llglobal =
        str::as_c_str(name,
                    {|buf|
                        lib::llvm::llvm::LLVMAddGlobal(ccx.llmod,
                                                       val_ty(llval), buf)
                    });
    lib::llvm::llvm::LLVMSetInitializer(llglobal, llval);
    lib::llvm::llvm::LLVMSetGlobalConstant(llglobal, True);

    if internal {
        lib::llvm::SetLinkage(llglobal, lib::llvm::InternalLinkage);
    }

    ret llglobal;
}


// Computes a set of variants of a enum that are guaranteed to have size and
// alignment at least as large as any other variant of the enum. This is an
// important performance optimization.

fn largest_variants(ccx: @crate_ctxt, tag_id: ast::def_id) -> [uint] {
    // Compute the minimum and maximum size and alignment for each variant.
    //
    // FIXME: We could do better here; e.g. we know that any variant that
    // contains (T,T) must be as least as large as any variant that contains
    // just T.
    let ranges = [];
    let variants = ty::enum_variants(ccx.tcx, tag_id);
    for variant: ty::variant_info in *variants {
        let bounded = true;
        let min_size = 0u, min_align = 0u;
        for elem_t: ty::t in variant.args {
            if ty::type_has_params(elem_t) {
                // FIXME: We could do better here; this causes us to
                // conservatively assume that (int, T) has minimum size 0,
                // when in fact it has minimum size sizeof(int).
                bounded = false;
            } else {
                let llty = type_of::type_of(ccx, elem_t);
                min_size += llsize_of_real(ccx, llty);
                min_align += llalign_of_real(ccx, llty);
            }
        }

        ranges +=
            [{size: {min: min_size, bounded: bounded},
              align: {min: min_align, bounded: bounded}}];
    }

    // Initialize the candidate set to contain all variants.
    let candidates = [mutable];
    for variant in *variants { candidates += [mutable true]; }

    // Do a pairwise comparison among all variants still in the candidate set.
    // Throw out any variant that we know has size and alignment at least as
    // small as some other variant.
    let i = 0u;
    while i < vec::len(ranges) - 1u {
        if candidates[i] {
            let j = i + 1u;
            while j < vec::len(ranges) {
                if candidates[j] {
                    if ranges[i].size.bounded && ranges[i].align.bounded &&
                           ranges[j].size.bounded && ranges[j].align.bounded {
                        if ranges[i].size >= ranges[j].size &&
                               ranges[i].align >= ranges[j].align {
                            // Throw out j.
                            candidates[j] = false;
                        } else if ranges[j].size >= ranges[i].size &&
                                      ranges[j].align >= ranges[j].align {
                            // Throw out i.
                            candidates[i] = false;
                        }
                    }
                }
                j += 1u;
            }
        }
        i += 1u;
    }

    // Return the resulting set.
    let result = [];
    i = 0u;
    while i < vec::len(candidates) {
        if candidates[i] { result += [i]; }
        i += 1u;
    }
    ret result;
}

fn round_up(size: u16, align: u8) -> u16 {
    assert (align >= 1u8);
    let alignment = align as u16;
    ret size - 1u16 + alignment & !(alignment - 1u16);
}

type size_align = {size: u16, align: u8};

fn compute_static_enum_size(ccx: @crate_ctxt, largest_variants: [uint],
                            did: ast::def_id) -> size_align {
    let max_size = 0u16;
    let max_align = 1u8;
    let variants = ty::enum_variants(ccx.tcx, did);
    for vid: uint in largest_variants {
        // We increment a "virtual data pointer" to compute the size.
        let lltys = [];
        for typ: ty::t in variants[vid].args {
            lltys += [type_of::type_of(ccx, typ)];
        }

        let llty = trans::common::T_struct(lltys);
        let dp = llsize_of_real(ccx, llty) as u16;
        let variant_align = llalign_of_real(ccx, llty) as u8;

        if max_size < dp { max_size = dp; }
        if max_align < variant_align { max_align = variant_align; }
    }

    // Add space for the enum if applicable.
    // FIXME (issue #792): This is wrong. If the enum starts with an 8 byte
    // aligned quantity, we don't align it.
    if vec::len(*variants) > 1u {
        let variant_t = T_enum_variant(ccx);
        max_size += llsize_of_real(ccx, variant_t) as u16;
        let align = llalign_of_real(ccx, variant_t) as u8;
        if max_align < align { max_align = align; }
    }

    ret {size: max_size, align: max_align};
}

enum enum_kind {
    tk_unit,    // 1 variant, no data
    tk_enum,    // N variants, no data
    tk_newtype, // 1 variant, data
    tk_complex  // N variants, no data
}

fn enum_kind(ccx: @crate_ctxt, did: ast::def_id) -> enum_kind {
    let variants = ty::enum_variants(ccx.tcx, did);
    if vec::any(*variants) {|v| vec::len(v.args) > 0u} {
        if vec::len(*variants) == 1u { tk_newtype }
        else { tk_complex }
    } else {
        if vec::len(*variants) <= 1u { tk_unit }
        else { tk_enum }
    }
}

// Returns the code corresponding to the pointer size on this architecture.
fn s_int(tcx: ty_ctxt) -> u8 {
    ret alt tcx.sess.targ_cfg.arch {
        session::arch_x86 { shape_i32 }
        session::arch_x86_64 { shape_i64 }
        session::arch_arm { shape_i32 }
    };
}

fn s_uint(tcx: ty_ctxt) -> u8 {
    ret alt tcx.sess.targ_cfg.arch {
        session::arch_x86 { shape_u32 }
        session::arch_x86_64 { shape_u64 }
        session::arch_arm { shape_u32 }
    };
}

fn s_float(tcx: ty_ctxt) -> u8 {
    ret alt tcx.sess.targ_cfg.arch {
        session::arch_x86 { shape_f64 }
        session::arch_x86_64 { shape_f64 }
        session::arch_arm { shape_f64 }
    };
}

fn s_variant_enum_t(tcx: ty_ctxt) -> u8 {
    ret s_int(tcx);
}

fn s_tydesc(_tcx: ty_ctxt) -> u8 {
    ret shape_tydesc;
}

fn s_send_tydesc(_tcx: ty_ctxt) -> u8 {
    ret shape_send_tydesc;
}

fn mk_ctxt(llmod: ModuleRef) -> ctxt {
    let llshapetablesty = trans::common::T_named_struct("shapes");
    let llshapetables = str::as_c_str("shapes", {|buf|
        lib::llvm::llvm::LLVMAddGlobal(llmod, llshapetablesty, buf)
    });

    ret {mutable next_tag_id: 0u16,
         pad: 0u16,
         tag_id_to_index: common::new_def_hash(),
         mutable tag_order: [],
         resources: interner::mk(hash_res_info, {|a, b| a == b}),
         llshapetablesty: llshapetablesty,
         llshapetables: llshapetables};
}

fn add_bool(&dest: [u8], val: bool) { dest += [if val { 1u8 } else { 0u8 }]; }

fn add_u16(&dest: [u8], val: u16) {
    dest += [(val & 0xffu16) as u8, (val >> 8u16) as u8];
}

fn add_substr(&dest: [u8], src: [u8]) {
    add_u16(dest, vec::len(src) as u16);
    dest += src;
}

fn shape_of(ccx: @crate_ctxt, t: ty::t, ty_param_map: [uint]) -> [u8] {
    alt ty::get(t).struct {
      ty::ty_nil | ty::ty_bool | ty::ty_uint(ast::ty_u8) |
      ty::ty_bot { [shape_u8] }
      ty::ty_int(ast::ty_i) { [s_int(ccx.tcx)] }
      ty::ty_float(ast::ty_f) { [s_float(ccx.tcx)] }
      ty::ty_uint(ast::ty_u) | ty::ty_ptr(_) { [s_uint(ccx.tcx)] }
      ty::ty_type { [s_tydesc(ccx.tcx)] }
      ty::ty_send_type { [s_send_tydesc(ccx.tcx)] }
      ty::ty_int(ast::ty_i8) { [shape_i8] }
      ty::ty_uint(ast::ty_u16) { [shape_u16] }
      ty::ty_int(ast::ty_i16) { [shape_i16] }
      ty::ty_uint(ast::ty_u32) { [shape_u32] }
      ty::ty_int(ast::ty_i32) | ty::ty_int(ast::ty_char) { [shape_i32] }
      ty::ty_uint(ast::ty_u64) { [shape_u64] }
      ty::ty_int(ast::ty_i64) { [shape_i64] }
      ty::ty_float(ast::ty_f32) { [shape_f32] }
      ty::ty_float(ast::ty_f64) { [shape_f64] }
      ty::ty_str {
        let s = [shape_vec];
        add_bool(s, true); // type is POD
        let unit_ty = ty::mk_mach_uint(ccx.tcx, ast::ty_u8);
        add_substr(s, shape_of(ccx, unit_ty, ty_param_map));
        s
      }
      ty::ty_enum(did, tps) {
        alt enum_kind(ccx, did) {
          // FIXME: For now we do this.
          tk_unit { [s_variant_enum_t(ccx.tcx)] }
          tk_enum { [s_variant_enum_t(ccx.tcx)] }
          tk_newtype | tk_complex {
            let s = [shape_enum], id;
            alt ccx.shape_cx.tag_id_to_index.find(did) {
              none {
                id = ccx.shape_cx.next_tag_id;
                ccx.shape_cx.tag_id_to_index.insert(did, id);
                ccx.shape_cx.tag_order += [did];
                ccx.shape_cx.next_tag_id += 1u16;
              }
              some(existing_id) { id = existing_id; }
            }
            add_u16(s, id as u16);

            add_u16(s, vec::len(tps) as u16);
            for tp: ty::t in tps {
                let subshape = shape_of(ccx, tp, ty_param_map);
                add_u16(s, vec::len(subshape) as u16);
                s += subshape;
            }
            s
          }
        }
      }
      ty::ty_box(_) | ty::ty_opaque_box { [shape_box] }
      ty::ty_uniq(mt) {
        let s = [shape_uniq];
        add_substr(s, shape_of(ccx, mt.ty, ty_param_map));
        s
      }
      ty::ty_vec(mt) {
        let s = [shape_vec];
        add_bool(s, ty::type_is_pod(ccx.tcx, mt.ty));
        add_substr(s, shape_of(ccx, mt.ty, ty_param_map));
        s
      }
      ty::ty_rec(fields) {
        let s = [shape_struct], sub = [];
        for f: field in fields {
            sub += shape_of(ccx, f.mt.ty, ty_param_map);
        }
        add_substr(s, sub);
        s
      }
      ty::ty_tup(elts) {
        let s = [shape_struct], sub = [];
        for elt in elts {
            sub += shape_of(ccx, elt, ty_param_map);
        }
        add_substr(s, sub);
        s
      }
      ty::ty_iface(_, _) { [shape_box_fn] }
      ty::ty_class(_, _) { [shape_class] }
      ty::ty_rptr(_, tm) {
        let s = [shape_rptr];
        add_substr(s, shape_of(ccx, tm.ty, ty_param_map));
        s
      }
      ty::ty_res(did, raw_subt, tps) {
        let subt = ty::substitute_type_params(ccx.tcx, tps, raw_subt);
        let ri = {did: did, tps: tps};
        let id = interner::intern(ccx.shape_cx.resources, ri);

        let s = [shape_res];
        add_u16(s, id as u16);
        add_u16(s, vec::len(tps) as u16);
        for tp: ty::t in tps {
            add_substr(s, shape_of(ccx, tp, ty_param_map));
        }
        add_substr(s, shape_of(ccx, subt, ty_param_map));
        s
      }
      ty::ty_param(n, _) {
        // Find the type parameter in the parameter list.
        alt vec::position_elt(ty_param_map, n) {
          some(i) { [shape_var, i as u8] }
          none { fail "ty param not found in ty_param_map"; }
        }
      }
      ty::ty_fn({proto: ast::proto_box, _}) { [shape_box_fn] }
      ty::ty_fn({proto: ast::proto_uniq, _}) { [shape_uniq_fn] }
      ty::ty_fn({proto: ast::proto_block, _}) |
      ty::ty_fn({proto: ast::proto_any, _}) { [shape_stack_fn] }
      ty::ty_fn({proto: ast::proto_bare, _}) { [shape_bare_fn] }
      ty::ty_opaque_closure_ptr(_) { [shape_opaque_closure_ptr] }
      ty::ty_constr(inner_t, _) { shape_of(ccx, inner_t, ty_param_map) }
      ty::ty_var(_) | ty::ty_self(_) {
        ccx.sess.bug("shape_of: unexpected type struct found");
      }
    }
}

// FIXME: We might discover other variants as we traverse these. Handle this.
fn shape_of_variant(ccx: @crate_ctxt, v: ty::variant_info,
                    ty_param_count: uint) -> [u8] {
    let ty_param_map = [];
    let i = 0u;
    while i < ty_param_count { ty_param_map += [i]; i += 1u; }

    let s = [];
    for t: ty::t in v.args { s += shape_of(ccx, t, ty_param_map); }
    ret s;
}

fn gen_enum_shapes(ccx: @crate_ctxt) -> ValueRef {
    // Loop over all the enum variants and write their shapes into a
    // data buffer. As we do this, it's possible for us to discover
    // new enums, so we must do this first.
    let i = 0u;
    let data = [];
    let offsets = [];
    while i < vec::len(ccx.shape_cx.tag_order) {
        let did = ccx.shape_cx.tag_order[i];
        let variants = ty::enum_variants(ccx.tcx, did);
        let item_tyt = ty::lookup_item_type(ccx.tcx, did);
        let ty_param_count = vec::len(*item_tyt.bounds);

        vec::iter(*variants) {|v|
            offsets += [vec::len(data) as u16];

            let variant_shape = shape_of_variant(ccx, v, ty_param_count);
            add_substr(data, variant_shape);

            let zname = str::bytes(v.name) + [0u8];
            add_substr(data, zname);
        }

        i += 1u;
    }

    // Now calculate the sizes of the header space (which contains offsets to
    // info records for each enum) and the info space (which contains offsets
    // to each variant shape). As we do so, build up the header.

    let header = [];
    let info = [];
    let header_sz = 2u16 * ccx.shape_cx.next_tag_id;
    let data_sz = vec::len(data) as u16;

    let info_sz = 0u16;
    for did_: ast::def_id in ccx.shape_cx.tag_order {
        let did = did_; // Satisfy alias checker.
        let num_variants = vec::len(*ty::enum_variants(ccx.tcx, did)) as u16;
        add_u16(header, header_sz + info_sz);
        info_sz += 2u16 * (num_variants + 2u16) + 3u16;
    }

    // Construct the info tables, which contain offsets to the shape of each
    // variant. Also construct the largest-variant table for each enum, which
    // contains the variants that the size-of operation needs to look at.

    let lv_table = [];
    i = 0u;
    for did_: ast::def_id in ccx.shape_cx.tag_order {
        let did = did_; // Satisfy alias checker.
        let variants = ty::enum_variants(ccx.tcx, did);
        add_u16(info, vec::len(*variants) as u16);

        // Construct the largest-variants table.
        add_u16(info,
                header_sz + info_sz + data_sz + (vec::len(lv_table) as u16));

        let lv = largest_variants(ccx, did);
        add_u16(lv_table, vec::len(lv) as u16);
        for v: uint in lv { add_u16(lv_table, v as u16); }

        // Determine whether the enum has dynamic size.
        let dynamic = vec::any(*variants, {|v|
            vec::any(v.args, {|t| ty::type_has_params(t)})
        });

        // If we can, write in the static size and alignment of the enum.
        // Otherwise, write a placeholder.
        let size_align = if dynamic { {size: 0u16, align: 0u8} }
                         else { compute_static_enum_size(ccx, lv, did) };
        // Write in the static size and alignment of the enum.
        add_u16(info, size_align.size);
        info += [size_align.align];

        // Now write in the offset of each variant.
        for v: ty::variant_info in *variants {
            add_u16(info, header_sz + info_sz + offsets[i]);
            i += 1u;
        }
    }

    assert (i == vec::len(offsets));
    assert (header_sz == vec::len(header) as u16);
    assert (info_sz == vec::len(info) as u16);
    assert (data_sz == vec::len(data) as u16);

    header += info;
    header += data;
    header += lv_table;

    ret mk_global(ccx, "tag_shapes", C_bytes(header), true);
}

fn gen_resource_shapes(ccx: @crate_ctxt) -> ValueRef {
    let dtors = [];
    let i = 0u;
    let len = interner::len(ccx.shape_cx.resources);
    while i < len {
        let ri = interner::get(ccx.shape_cx.resources, i);
        dtors += [trans::base::get_res_dtor(ccx, ri.did, ri.tps)];
        i += 1u;
    }

    ret mk_global(ccx, "resource_shapes", C_struct(dtors), true);
}

fn gen_shape_tables(ccx: @crate_ctxt) {
    let lltagstable = gen_enum_shapes(ccx);
    let llresourcestable = gen_resource_shapes(ccx);
    trans::common::set_struct_body(ccx.shape_cx.llshapetablesty,
                                   [val_ty(lltagstable),
                                    val_ty(llresourcestable)]);

    let lltables =
        C_named_struct(ccx.shape_cx.llshapetablesty,
                       [lltagstable, llresourcestable]);
    lib::llvm::llvm::LLVMSetInitializer(ccx.shape_cx.llshapetables, lltables);
    lib::llvm::llvm::LLVMSetGlobalConstant(ccx.shape_cx.llshapetables, True);
    lib::llvm::SetLinkage(ccx.shape_cx.llshapetables,
                          lib::llvm::InternalLinkage);
}

// ______________________________________________________________________
// compute sizeof / alignof

type metrics = {
    bcx: block,
    sz: ValueRef,
    align: ValueRef
};

type tag_metrics = {
    bcx: block,
    sz: ValueRef,
    align: ValueRef,
    payload_align: ValueRef
};

// Returns the real size of the given type for the current target.
fn llsize_of_real(cx: @crate_ctxt, t: TypeRef) -> uint {
    ret llvm::LLVMStoreSizeOfType(cx.td.lltd, t) as uint;
}

// Returns the real alignment of the given type for the current target.
fn llalign_of_real(cx: @crate_ctxt, t: TypeRef) -> uint {
    ret llvm::LLVMPreferredAlignmentOfType(cx.td.lltd, t) as uint;
}

fn llsize_of(cx: @crate_ctxt, t: TypeRef) -> ValueRef {
    ret llvm::LLVMConstIntCast(lib::llvm::llvm::LLVMSizeOf(t), cx.int_type,
                               False);
}

fn llalign_of(cx: @crate_ctxt, t: TypeRef) -> ValueRef {
    ret llvm::LLVMConstIntCast(lib::llvm::llvm::LLVMAlignOf(t), cx.int_type,
                               False);
}

// Computes the static size of a enum, without using mk_tup(), which is
// bad for performance.
//
// FIXME: Migrate trans over to use this.

// Computes the size of the data part of an enum.
fn static_size_of_enum(cx: @crate_ctxt, t: ty::t) -> uint {
    if cx.enum_sizes.contains_key(t) { ret cx.enum_sizes.get(t); }
    alt ty::get(t).struct {
      ty::ty_enum(tid, subtys) {
        // Compute max(variant sizes).
        let max_size = 0u;
        let variants = ty::enum_variants(cx.tcx, tid);
        for variant: ty::variant_info in *variants {
            let tup_ty = simplify_type(cx.tcx,
                                       ty::mk_tup(cx.tcx, variant.args));
            // Perform any type parameter substitutions.
            tup_ty = ty::substitute_type_params(cx.tcx, subtys, tup_ty);
            // Here we possibly do a recursive call.
            let this_size =
                llsize_of_real(cx, type_of::type_of(cx, tup_ty));
            if max_size < this_size { max_size = this_size; }
        }
        cx.enum_sizes.insert(t, max_size);
        ret max_size;
      }
      _ { cx.sess.bug("static_size_of_enum called on non-enum"); }
    }
}

// Creates a simpler, size-equivalent type. The resulting type is guaranteed
// to have (a) the same size as the type that was passed in; (b) to be non-
// recursive. This is done by replacing all boxes in a type with boxed unit
// types.
// This should reduce all pointers to some simple pointer type, to
// ensure that we don't recurse endlessly when computing the size of a
// nominal type that has pointers to itself in it.
fn simplify_type(tcx: ty::ctxt, typ: ty::t) -> ty::t {
    fn nilptr(tcx: ty::ctxt) -> ty::t {
        ty::mk_ptr(tcx, {ty: ty::mk_nil(tcx), mutbl: ast::m_imm})
    }
    fn simplifier(tcx: ty::ctxt, typ: ty::t) -> ty::t {
        alt ty::get(typ).struct {
          ty::ty_box(_) | ty::ty_opaque_box | ty::ty_uniq(_) | ty::ty_vec(_) |
          ty::ty_ptr(_) { nilptr(tcx) }
          ty::ty_fn(_) { ty::mk_tup(tcx, [nilptr(tcx), nilptr(tcx)]) }
          ty::ty_res(_, sub, tps) {
            let sub1 = ty::substitute_type_params(tcx, tps, sub);
            ty::mk_tup(tcx, [ty::mk_int(tcx), simplify_type(tcx, sub1)])
          }
          _ { typ }
        }
    }
    ty::fold_ty(tcx, ty::fm_general(bind simplifier(tcx, _)), typ)
}

// Given a tag type `ty`, returns the offset of the payload.
//fn tag_payload_offs(bcx: block, tag_id: ast::def_id, tps: [ty::t])
//    -> ValueRef {
//    alt tag_kind(tag_id) {
//      tk_unit | tk_enum | tk_newtype { C_int(bcx.ccx(), 0) }
//      tk_complex {
//        compute_tag_metrics(tag_id, tps)
//      }
//    }
//}
