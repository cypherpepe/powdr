use std::prover::Query;
use std::convert::fe;
use std::protocols::lookup::lookup;
use std::protocols::lookup::compute_next_z;
use std::math::fp2::Fp2;

machine Main with degree: 8 {
    col fixed a = [1, 1, 4, 1, 1, 2, 1, 1];
    col witness b(i) query Query::Hint(fe(i+1));
    col fixed m = [6, 1, 0, 1, 0, 0, 0, 0];

    let lookup_constraint = Constr::Lookup(
        (Option::None, Option::None),
        [(a, b)]
    );

    // TODO: Functions currently cannot add witness columns at later stages,
    // so we have to manually create it here and pass it to lookup(). 
    col witness stage(1) z1;
    col witness stage(1) z2;

    let is_first: col = |i| if i == 0 { 1 } else { 0 };
    lookup(is_first, [z1, z2], lookup_constraint, m);

    // TODO: Helper columns, because we can't access the previous row in hints
    let hint = query |i| Query::Hint(compute_next_z(Fp2::Fp2(z1, z2), lookup_constraint, m)[i]); 
    col witness stage(1) z1_next(i) query hint(0);
    col witness stage(1) z2_next(i) query hint(1);

    z1' = z1_next;
    z2' = z2_next;
}