use crate::circuit_builder::PowdrAir;
use crate::stark::folder::ProverConstraintFolder;
use crate::symbolic_builder::get_max_constraint_degree;
use crate::symbolic_builder::SymbolicAirBuilder;
use itertools::izip;
use itertools::Itertools;
use p3_air::{Air, TwoRowMatrixView};
use p3_challenger::{CanObserve, CanSample, FieldChallenger};
use p3_commit::{Pcs, PolynomialSpace};
use p3_field::{AbstractExtensionField, AbstractField, PackedValue};
use p3_matrix::MatrixGet;
use p3_matrix::{dense::RowMajorMatrix, Matrix};
use p3_uni_stark::{Domain, Val};
use p3_uni_stark::{PackedChallenge, PackedVal, StarkGenericConfig};
use p3_util::log2_ceil_usize;
use p3_util::log2_strict_usize;
use rayon::iter::{IndexedParallelIterator, IntoParallelIterator, ParallelIterator};

use tracing::info_span;

use super::params::{Commitments, OpenedValues, Proof, StarkProvingKey};

pub fn prove<SC, A>(
    config: &SC,
    proving_key: Option<&StarkProvingKey<SC>>,
    air: &A,
    challenger: &mut SC::Challenger,
    trace: RowMajorMatrix<Val<SC>>,
    public_values: &Vec<Val<SC>>,
) -> Proof<SC>
where
    SC: StarkGenericConfig,
    A: PowdrAir<SymbolicAirBuilder<Val<SC>>> + for<'a> PowdrAir<ProverConstraintFolder<'a, SC>>,
{
    let degree = trace.height();
    let log_degree = log2_strict_usize(degree);

    let constraint_degree = get_max_constraint_degree(air, public_values.len());
    let log_quotient_degree = log2_ceil_usize(constraint_degree - 1);
    let quotient_degree = 1 << log_quotient_degree;

    let pcs = config.pcs();
    let trace_domain = pcs.natural_domain_for_degree(degree);

    let (trace_commit, trace_data) =
        info_span!("commit to trace data").in_scope(|| pcs.commit(vec![(trace_domain, trace)]));

    challenger.observe(trace_commit.clone());
    let alpha: SC::Challenge = challenger.sample_ext_element();

    let quotient_domain =
        trace_domain.create_disjoint_domain(1 << (log_degree + log_quotient_degree));

    let fixed_on_quotient_domain = proving_key.map(|proving_key| {
        pcs.get_evaluations_on_domain(&proving_key.fixed_data, 0, quotient_domain)
    });

    let trace_on_quotient_domain = pcs.get_evaluations_on_domain(&trace_data, 0, quotient_domain);

    let quotient_values = quotient_values(
        air,
        public_values,
        trace_domain,
        quotient_domain,
        fixed_on_quotient_domain,
        trace_on_quotient_domain,
        alpha,
    );
    let quotient_flat = RowMajorMatrix::new_col(quotient_values).flatten_to_base();
    let quotient_chunks = quotient_domain.split_evals(quotient_degree, quotient_flat);
    let qc_domains = quotient_domain.split_domains(quotient_degree);

    let (quotient_commit, quotient_data) = info_span!("commit to quotient poly chunks")
        .in_scope(|| pcs.commit(izip!(qc_domains, quotient_chunks).collect_vec()));
    challenger.observe(quotient_commit.clone());

    let commitments = Commitments {
        trace: trace_commit,
        quotient_chunks: quotient_commit,
    };

    let zeta: SC::Challenge = challenger.sample();
    let zeta_next = trace_domain.next_point(zeta).unwrap();

    let (opened_values, opening_proof) = pcs.open(
        // only open fixed commitments in the presence of a proving key
        proving_key
            .map(|proving_key| (&proving_key.fixed_data, vec![vec![zeta, zeta_next]]))
            .into_iter()
            .chain([
                (&trace_data, vec![vec![zeta, zeta_next]]),
                (
                    &quotient_data,
                    // open every chunk at zeta
                    (0..quotient_degree).map(|_| vec![zeta]).collect_vec(),
                ),
            ])
            .collect(),
        challenger,
    );

    let mut opened_values = opened_values.iter();

    // maybe get values for the fixed columns
    let (fixed_local, fixed_next) = if proving_key.is_some() {
        let value = opened_values.next().unwrap();
        assert_eq!(value.len(), 1);
        assert_eq!(value[0].len(), 2);
        (value[0][0].clone(), value[0][1].clone())
    } else {
        (vec![], vec![])
    };

    // get values for the trace
    let value = opened_values.next().unwrap();
    assert_eq!(value.len(), 1);
    assert_eq!(value[0].len(), 2);
    let trace_local = value[0][0].clone();
    let trace_next = value[0][1].clone();

    // get values for the quotient
    let value = opened_values.next().unwrap();
    assert_eq!(value.len(), quotient_degree);
    let quotient_chunks = value.iter().map(|v| v[0].clone()).collect_vec();

    let opened_values = OpenedValues {
        trace_local,
        trace_next,
        fixed_local,
        fixed_next,
        quotient_chunks,
    };
    Proof {
        commitments,
        opened_values,
        opening_proof,
        degree_bits: log_degree,
    }
}

fn quotient_values<SC, A, Mat>(
    air: &A,
    public_values: &Vec<Val<SC>>,
    trace_domain: Domain<SC>,
    quotient_domain: Domain<SC>,
    fixed_on_quotient_domain: Option<Mat>,
    trace_on_quotient_domain: Mat,
    alpha: SC::Challenge,
) -> Vec<SC::Challenge>
where
    SC: StarkGenericConfig,
    A: for<'a> Air<ProverConstraintFolder<'a, SC>>,
    Mat: MatrixGet<Val<SC>> + Sync,
{
    let quotient_size = quotient_domain.size();
    let fixed_width = fixed_on_quotient_domain
        .as_ref()
        .map(Matrix::width)
        .unwrap_or_default();
    let width = trace_on_quotient_domain.width();
    let sels = trace_domain.selectors_on_coset(quotient_domain);

    let qdb = log2_strict_usize(quotient_domain.size()) - log2_strict_usize(trace_domain.size());
    let next_step = 1 << qdb;

    assert!(quotient_size >= PackedVal::<SC>::WIDTH);

    (0..quotient_size)
        .into_par_iter()
        .step_by(PackedVal::<SC>::WIDTH)
        .flat_map_iter(|i_start| {
            let wrap = |i| i % quotient_size;
            let i_range = i_start..i_start + PackedVal::<SC>::WIDTH;

            let is_first_row = *PackedVal::<SC>::from_slice(&sels.is_first_row[i_range.clone()]);
            let is_last_row = *PackedVal::<SC>::from_slice(&sels.is_last_row[i_range.clone()]);
            let is_transition = *PackedVal::<SC>::from_slice(&sels.is_transition[i_range.clone()]);
            let inv_zeroifier = *PackedVal::<SC>::from_slice(&sels.inv_zeroifier[i_range.clone()]);

            let fixed_local = (0..fixed_width)
                .map(|col| {
                    PackedVal::<SC>::from_fn(|offset| {
                        fixed_on_quotient_domain
                            .as_ref()
                            .unwrap()
                            .get(wrap(i_start + offset), col)
                    })
                })
                .collect_vec();

            let fixed_next = (0..fixed_width)
                .map(|col| {
                    PackedVal::<SC>::from_fn(|offset| {
                        fixed_on_quotient_domain
                            .as_ref()
                            .unwrap()
                            .get(wrap(i_start + next_step + offset), col)
                    })
                })
                .collect_vec();

            let local = (0..width)
                .map(|col| {
                    PackedVal::<SC>::from_fn(|offset| {
                        trace_on_quotient_domain.get(wrap(i_start + offset), col)
                    })
                })
                .collect_vec();

            let next = (0..width)
                .map(|col| {
                    PackedVal::<SC>::from_fn(|offset| {
                        trace_on_quotient_domain.get(wrap(i_start + next_step + offset), col)
                    })
                })
                .collect_vec();

            let accumulator = PackedChallenge::<SC>::zero();
            let mut folder = ProverConstraintFolder {
                main: TwoRowMatrixView {
                    local: &local,
                    next: &next,
                },
                fixed: TwoRowMatrixView {
                    local: &fixed_local,
                    next: &fixed_next,
                },
                public_values,
                is_first_row,
                is_last_row,
                is_transition,
                alpha,
                accumulator,
            };
            air.eval(&mut folder);

            // quotient(x) = constraints(x) / Z_H(x)
            let quotient = folder.accumulator * inv_zeroifier;

            // "Transpose" D packed base coefficients into WIDTH scalar extension coefficients.
            (0..PackedVal::<SC>::WIDTH).map(move |idx_in_packing| {
                let quotient_value = (0..<SC::Challenge as AbstractExtensionField<Val<SC>>>::D)
                    .map(|coeff_idx| quotient.as_base_slice()[coeff_idx].as_slice()[idx_in_packing])
                    .collect::<Vec<_>>();
                SC::Challenge::from_base_slice(&quotient_value)
            })
        })
        .collect()
}
