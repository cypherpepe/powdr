use itertools::Itertools;
use p3_air::{Air, BaseAir, TwoRowMatrixView};
use p3_challenger::{CanObserve, CanSample, FieldChallenger};
use p3_commit::{Pcs, PolynomialSpace};
use p3_field::{AbstractExtensionField, AbstractField, Field};
use p3_uni_stark::{
    get_log_quotient_degree, StarkGenericConfig, SymbolicAirBuilder, Val, VerificationError,
};

use super::{
    folder::VerifierConstraintFolder,
    params::{Proof, StarkVerifyingKey},
};

pub fn verify<SC, A>(
    config: &SC,
    verifying_key: Option<&StarkVerifyingKey<SC>>,
    air: &A,
    challenger: &mut SC::Challenger,
    proof: &Proof<SC>,
    public_values: &Vec<Val<SC>>,
) -> Result<(), VerificationError>
where
    SC: StarkGenericConfig,
    A: Air<SymbolicAirBuilder<Val<SC>>> + for<'a> Air<VerifierConstraintFolder<'a, SC>>,
{
    let verifying_key = verifying_key.expect("fixed please");

    let Proof {
        commitments,
        opened_values,
        opening_proof,
        degree_bits,
    } = proof;

    let degree = 1 << degree_bits;
    let log_quotient_degree = get_log_quotient_degree::<Val<SC>, A>(air, public_values.len());
    let quotient_degree = 1 << log_quotient_degree;

    let pcs = config.pcs();
    let trace_domain = pcs.natural_domain_for_degree(degree);
    let quotient_domain =
        trace_domain.create_disjoint_domain(1 << (degree_bits + log_quotient_degree));
    let quotient_chunks_domains = quotient_domain.split_domains(quotient_degree);

    let air_width = <A as BaseAir<Val<SC>>>::width(air);
    let valid_shape = opened_values.trace_local.len() == air_width
        && opened_values.trace_next.len() == air_width
        && opened_values.quotient_chunks.len() == quotient_degree
        && opened_values
            .quotient_chunks
            .iter()
            .all(|qc| qc.len() == <SC::Challenge as AbstractExtensionField<Val<SC>>>::D);
    if !valid_shape {
        return Err(VerificationError::InvalidProofShape);
    }

    challenger.observe(commitments.trace.clone());
    let alpha: SC::Challenge = challenger.sample_ext_element();
    challenger.observe(commitments.quotient_chunks.clone());

    let zeta: SC::Challenge = challenger.sample();
    let zeta_next = trace_domain.next_point(zeta).unwrap();

    pcs.verify(
        vec![
            (
                verifying_key.fixed_commit.clone(),
                (vec![(
                    trace_domain,
                    vec![
                        (zeta, opened_values.fixed_local.clone()),
                        (zeta_next, opened_values.fixed_next.clone()),
                    ],
                )]),
            ),
            (
                commitments.trace.clone(),
                vec![(
                    trace_domain,
                    vec![
                        (zeta, opened_values.trace_local.clone()),
                        (zeta_next, opened_values.trace_next.clone()),
                    ],
                )],
            ),
            (
                commitments.quotient_chunks.clone(),
                quotient_chunks_domains
                    .iter()
                    .zip(&opened_values.quotient_chunks)
                    .map(|(domain, values)| (*domain, vec![(zeta, values.clone())]))
                    .collect_vec(),
            ),
        ],
        opening_proof,
        challenger,
    )
    .map_err(|_| VerificationError::InvalidOpeningArgument)?;

    let zps = quotient_chunks_domains
        .iter()
        .enumerate()
        .map(|(i, domain)| {
            quotient_chunks_domains
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, other_domain)| {
                    other_domain.zp_at_point(zeta)
                        * other_domain.zp_at_point(domain.first_point()).inverse()
                })
                .product::<SC::Challenge>()
        })
        .collect_vec();

    let quotient = opened_values
        .quotient_chunks
        .iter()
        .enumerate()
        .map(|(ch_i, ch)| {
            ch.iter()
                .enumerate()
                .map(|(e_i, &c)| zps[ch_i] * SC::Challenge::monomial(e_i) * c)
                .sum::<SC::Challenge>()
        })
        .sum::<SC::Challenge>();

    let sels = trace_domain.selectors_at_point(zeta);

    let mut folder = VerifierConstraintFolder {
        main: TwoRowMatrixView {
            local: &opened_values.trace_local,
            next: &opened_values.trace_next,
        },
        fixed: TwoRowMatrixView {
            local: &opened_values.fixed_local,
            next: &opened_values.fixed_next,
        },
        public_values,
        is_first_row: sels.is_first_row,
        is_last_row: sels.is_last_row,
        is_transition: sels.is_transition,
        alpha,
        accumulator: SC::Challenge::zero(),
    };
    air.eval(&mut folder);
    let folded_constraints = folder.accumulator;

    // Finally, check that
    //     folded_constraints(zeta) / Z_H(zeta) = quotient(zeta)
    if folded_constraints * sels.inv_zeroifier != quotient {
        return Err(VerificationError::OodEvaluationMismatch);
    }

    Ok(())
}
