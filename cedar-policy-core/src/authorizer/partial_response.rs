use std::collections::HashMap;

use either::Either;
use smol_str::SmolStr;
use std::sync::Arc;

use super::{
    Annotations, AuthorizationError, Authorizer, Decision, Effect, Expr, Policy, PolicySet,
    PolicySetError, Request, Response, Value,
};
use crate::{ast::PolicyID, entities::Entities, evaluator::EvaluationError};

type PolicyPrototype<'a> = (Effect, &'a PolicyID, &'a Arc<Expr>, &'a Arc<Annotations>);

/// Enum representing whether a policy is not satisfied due to
/// evaluating to `false`, or because it errored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ErrorState {
    /// The policy did not error
    NoError,
    /// The policy did error
    Error,
}

/// A partially evaluated authorization response.
/// Splits the results into several categories: satisfied, false, and residual for each policy effect.
/// Also tracks all the errors that were encountered during evaluation.
/// This structure currently has to own all of the `PolicyID` objects due to the [`Self::reauthorize`]
/// method. If [`PolicySet`] could borrow its PolicyID/contents then this whole structured could be borrowed.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct PartialResponse {
    /// All of the [`Effect::Permit`] policies that were satisfied
    pub satisfied_permits: HashMap<PolicyID, Arc<Annotations>>,
    /// All of the [`Effect::Permit`] policies that were not satisfied
    pub false_permits: HashMap<PolicyID, (ErrorState, Arc<Annotations>)>,
    /// All of the [`Effect::Permit`] policies that evaluated to a residual
    pub residual_permits: HashMap<PolicyID, (Arc<Expr>, Arc<Annotations>)>,
    /// All of the [`Effect::Forbid`] policies that were satisfied
    pub satisfied_forbids: HashMap<PolicyID, Arc<Annotations>>,
    /// All of the [`Effect::Forbid`] policies that were not satisfied
    pub false_forbids: HashMap<PolicyID, (ErrorState, Arc<Annotations>)>,
    /// All of the [`Effect::Forbid`] policies that evaluated to a residual
    pub residual_forbids: HashMap<PolicyID, (Arc<Expr>, Arc<Annotations>)>,
    /// All of the policy errors encountered during evaluation
    pub errors: Vec<AuthorizationError>,
    /// The trivial `true` expression, used for materializing a residual for satisfied policies
    true_expr: Arc<Expr>,
    /// The trivial `false` expression, used for materializing a residual for non-satisfied policies
    false_expr: Arc<Expr>,
}

impl PartialResponse {
    /// Create a partial response from each of the policy result categories
    pub fn new(
        true_permits: impl IntoIterator<Item = (PolicyID, Arc<Annotations>)>,
        false_permits: impl IntoIterator<Item = (PolicyID, (ErrorState, Arc<Annotations>))>,
        residual_permits: impl IntoIterator<Item = (PolicyID, (Arc<Expr>, Arc<Annotations>))>,
        true_forbids: impl IntoIterator<Item = (PolicyID, Arc<Annotations>)>,
        false_forbids: impl IntoIterator<Item = (PolicyID, (ErrorState, Arc<Annotations>))>,
        residual_forbids: impl IntoIterator<Item = (PolicyID, (Arc<Expr>, Arc<Annotations>))>,
        errors: impl IntoIterator<Item = AuthorizationError>,
    ) -> Self {
        Self {
            satisfied_permits: true_permits.into_iter().collect(),
            false_permits: false_permits.into_iter().collect(),
            residual_permits: residual_permits.into_iter().collect(),
            satisfied_forbids: true_forbids.into_iter().collect(),
            false_forbids: false_forbids.into_iter().collect(),
            residual_forbids: residual_forbids.into_iter().collect(),
            errors: errors.into_iter().collect(),
            true_expr: Arc::new(Expr::val(true)),
            false_expr: Arc::new(Expr::val(false)),
        }
    }

    /// Convert this response into a concrete evaluation response.
    /// All residuals are treated as errors
    pub fn concretize(self) -> Response {
        self.into()
    }

    /// Attempt to reach a partial decision; the presence of residuals may result in returning [`None`],
    /// indicating that a decision could not be reached given the unknowns
    pub fn decision(&self) -> Option<Decision> {
        match (
            !self.satisfied_forbids.is_empty(),
            !self.satisfied_permits.is_empty(),
            !self.residual_permits.is_empty(),
            !self.residual_forbids.is_empty(),
        ) {
            // Any true forbids means we will deny
            (true, _, _, _) => Some(Decision::Deny),
            // No potentially or trivially true permits, means we default deny
            (_, false, false, _) => Some(Decision::Deny),
            // Potentially true forbids, means we can't know (as that forbid may evaluate to true, overriding any permits)
            (false, _, _, true) => None,
            // No true permits, but some potentially true permits + no true/potentially true forbids means we don't know
            (false, false, true, false) => None,
            // At least one trivially true permit, and no trivially or possible true forbids, means we allow
            (false, true, _, false) => Some(Decision::Allow),
        }
    }

    /// All of the [`Effect::Permit`] policies that were known to be satisfied
    fn definitely_satisfied_permits(&self) -> impl Iterator<Item = &PolicyID> {
        self.satisfied_permits.iter().map(first)
    }

    /// All of the [`Effect::Forbid`] policies that were known to be satisfied
    fn definitely_satisfied_forbids(&self) -> impl Iterator<Item = &PolicyID> {
        self.satisfied_forbids.iter().map(first)
    }

    /// Returns the set of [`PolicyID`]s that were definitely satisfied -- both permits and forbids
    pub fn definitely_satisfied(&self) -> impl Iterator<Item = &PolicyID> {
        self.definitely_satisfied_permits()
            .chain(self.definitely_satisfied_forbids())
    }

    /// Returns the set of [`PolicyID`]s that encountered errors
    pub fn definitely_errored(&self) -> impl Iterator<Item = &PolicyID> {
        self.false_permits
            .iter()
            .chain(self.false_forbids.iter())
            .filter_map(did_error)
    }

    /// Returns an over-approximation of the set of determining policies.
    ///
    /// This is all policies that may be determining for any substitution of the unknowns.
    pub fn may_be_determining(&self) -> impl Iterator<Item = &PolicyID> {
        if self.satisfied_forbids.is_empty() {
            // We have no definitely true forbids, so the over approx is everything that is true or potentially true
            Either::Left(
                self.definitely_satisfied_permits()
                    .chain(self.residual_permits.keys())
                    .chain(self.residual_forbids.keys()),
            )
        } else {
            // We have definitely true forbids, so we know only things that can determine is
            // true forbids and potentially true forbids
            Either::Right(
                self.definitely_satisfied_forbids()
                    .chain(self.residual_forbids.keys()),
            )
        }
    }

    /// Returns an under-approximation of the set of determining policies.
    ///
    /// This is all policies that must be determining for all possible substitutions of the unknowns.
    pub fn must_be_determining(&self) -> impl Iterator<Item = &PolicyID> {
        // If there are no true forbids or potentially true forbids,
        // then the under approximation is the true permits
        if self.satisfied_forbids.is_empty() && self.residual_forbids.is_empty() {
            Either::Left(self.definitely_satisfied_permits())
        } else {
            // Otherwise it's the true forbids
            Either::Right(self.definitely_satisfied_forbids())
        }
    }

    /// Returns the set of non-trivial (meaning more than just `true` or `false`) residuals expressions
    pub fn nontrivial_residuals(&'_ self) -> impl Iterator<Item = Policy> + '_ {
        self.nontrival_permits().chain(self.nontrival_forbids())
    }

    /// Returns the set of ids of non-trivial (meaning more than just `true` or `false`) residuals expressions
    pub fn nontrivial_residual_ids(&self) -> impl Iterator<Item = &PolicyID> {
        self.residual_permits
            .keys()
            .chain(self.residual_forbids.keys())
    }

    /// Returns the set of non-trivial (meaning more than just `true` or `false`) residuals expressions from [`Effect::Permit`]
    fn nontrival_permits(&self) -> impl Iterator<Item = Policy> + '_ {
        self.residual_permits
            .iter()
            .map(|(id, (expr, annotations))| {
                construct_policy((Effect::Permit, id, expr, annotations))
            })
    }

    /// Returns the set of non-trivial (meaning more than just `true` or `false`) residuals expressions from [`Effect::Forbid`]
    pub fn nontrival_forbids(&self) -> impl Iterator<Item = Policy> + '_ {
        self.residual_forbids
            .iter()
            .map(|(id, (expr, annotations))| {
                construct_policy((Effect::Forbid, id, expr, annotations))
            })
    }

    /// Returns every policy residual, including trivial ones
    pub fn all_residuals(&'_ self) -> impl Iterator<Item = Policy> + '_ {
        self.all_permit_residuals()
            .chain(self.all_forbid_residuals())
            .map(construct_policy)
    }

    /// Returns all residuals expressions that come from [`Effect::Permit`] policies
    fn all_permit_residuals(&'_ self) -> impl Iterator<Item = PolicyPrototype<'_>> {
        let trues = self
            .satisfied_permits
            .iter()
            .map(|(id, a)| (id, (&self.true_expr, a)));
        let falses = self
            .false_permits
            .iter()
            .map(|(id, (_, a))| (id, (&self.false_expr, a)));
        let nontrivial = self
            .residual_permits
            .iter()
            .map(|(id, (r, a))| (id, (r, a)));
        trues
            .chain(falses)
            .chain(nontrivial)
            .map(|(id, (r, a))| (Effect::Permit, id, r, a))
    }

    /// Returns all residuals expressions that come from [`Effect::Forbid`] policies
    fn all_forbid_residuals(&'_ self) -> impl Iterator<Item = PolicyPrototype<'_>> {
        let trues = self
            .satisfied_forbids
            .iter()
            .map(|(id, a)| (id, (&self.true_expr, a)));
        let falses = self
            .false_forbids
            .iter()
            .map(|(id, (_, a))| (id, (&self.false_expr, a)));
        let nontrivial = self
            .residual_forbids
            .iter()
            .map(|(id, (r, a))| (id, (r, a)));
        trues
            .chain(falses)
            .chain(nontrivial)
            .map(|(id, (r, a))| (Effect::Forbid, id, r, a))
    }

    /// Return the residual for a given [`PolicyID`], if it exists in the response
    pub fn get(&self, id: &PolicyID) -> Option<&Expr> {
        self.get_true(id)
            .or_else(|| self.get_false(id).or_else(|| self.get_residual(id)))
    }

    /// Get a policy that evaluated to a true residual, if it did so
    fn get_true(&self, id: &PolicyID) -> Option<&Expr> {
        self.satisfied_permits
            .get(id)
            .or_else(|| self.satisfied_forbids.get(id))
            .map(|_| self.true_expr.as_ref())
    }

    /// Get a policy that evaluated to a false residual, if it did so
    fn get_false(&self, id: &PolicyID) -> Option<&Expr> {
        self.false_permits
            .get(id)
            .or_else(|| self.false_forbids.get(id))
            .map(|_| self.false_expr.as_ref())
    }

    /// Get a policy that evaluated to a non-trivial residual, if it did so
    fn get_residual(&self, id: &PolicyID) -> Option<&Expr> {
        self.residual_permits
            .get(id)
            .or_else(|| self.residual_forbids.get(id))
            .map(|(r, _)| r.as_ref())
    }

    /// Attempt to re-authorize this response given a mapping from unknowns to values
    pub fn reauthorize(
        &self,
        mapping: &HashMap<SmolStr, Value>,
        auth: &Authorizer,
        r: Request,
        es: &Entities,
    ) -> Result<Self, PolicySetError> {
        let policyset = self.all_policies(mapping)?;
        Ok(auth.is_authorized_core(r, &policyset, es))
    }

    fn all_policies(&self, mapping: &HashMap<SmolStr, Value>) -> Result<PolicySet, PolicySetError> {
        let mapper = map_unknowns(mapping);
        PolicySet::try_from_iter(
            self.all_permit_residuals()
                .chain(self.all_forbid_residuals())
                .map(mapper),
        )
    }

    fn errors(self) -> impl Iterator<Item = AuthorizationError> {
        self.residual_forbids
            .into_iter()
            .chain(self.residual_permits)
            .map(
                |(id, (expr, _))| AuthorizationError::PolicyEvaluationError {
                    id: id.clone(),
                    error: EvaluationError::non_value(expr.as_ref().clone()),
                },
            )
            .chain(self.errors)
            .collect::<Vec<_>>()
            .into_iter()
    }
}

impl From<PartialResponse> for Response {
    fn from(p: PartialResponse) -> Self {
        let decision = if !p.satisfied_permits.is_empty() && p.satisfied_forbids.is_empty() {
            Decision::Allow
        } else {
            Decision::Deny
        };
        Response::new(
            decision,
            p.must_be_determining().cloned().collect(),
            p.errors().collect(),
        )
    }
}

/// Build a policy from a policy prototype
fn construct_policy((effect, id, expr, annotations): PolicyPrototype<'_>) -> Policy {
    Policy::from_when_clause_annos(effect, expr.clone(), id.clone(), (*annotations).clone())
}

/// Given a mapping from unknown names to values and a policy prototype
/// substitute the residual with the mapping and build a policy.
/// Curried for convenience
fn map_unknowns<'a>(
    mapping: &'a HashMap<SmolStr, Value>,
) -> impl Fn(PolicyPrototype<'a>) -> Policy {
    |(effect, id, expr, annotations)| {
        Policy::from_when_clause_annos(
            effect,
            Arc::new(expr.substitute(mapping)),
            id.clone(),
            annotations.clone(),
        )
    }
}

/// Checks if a given residual record did error, returning the [`PolicyID`] if it did
fn did_error<'a>(
    (id, (state, _)): (&'a PolicyID, &'_ (ErrorState, Arc<Annotations>)),
) -> Option<&'a PolicyID> {
    match *state {
        ErrorState::NoError => None,
        ErrorState::Error => Some(id),
    }
}

/// Extract the first element from a tuple
fn first<A, B>(p: (A, B)) -> A {
    p.0
}

#[cfg(test)]
// PANIC SAFETY testing
#[allow(clippy::indexing_slicing)]
mod test {
    use std::{
        collections::HashSet,
        iter::{empty, once},
    };

    // An extremely slow and bad set, but it only requires that the contents be [`PartialEq`]
    // Using this because I don't want to enforce an output order on the tests, but policies can't easily be Hash or Ord
    #[derive(Debug, Default)]
    struct SlowSet<T> {
        contents: Vec<T>,
    }

    impl<T: PartialEq> SlowSet<T> {
        pub fn from(iter: impl IntoIterator<Item = T>) -> Self {
            let mut contents = vec![];
            for item in iter.into_iter() {
                if !contents.contains(&item) {
                    contents.push(item)
                }
            }
            Self { contents }
        }

        pub fn len(&self) -> usize {
            self.contents.len()
        }

        pub fn contains(&self, item: &T) -> bool {
            self.contents.contains(item)
        }
    }

    impl<T: PartialEq> PartialEq for SlowSet<T> {
        fn eq(&self, rhs: &Self) -> bool {
            if self.len() == rhs.len() {
                self.contents.iter().all(|item| rhs.contains(item))
            } else {
                false
            }
        }
    }

    impl<T: PartialEq> FromIterator<T> for SlowSet<T> {
        fn from_iter<I>(iter: I) -> Self
        where
            I: IntoIterator<Item = T>,
        {
            Self::from(iter)
        }
    }

    use crate::authorizer::{ActionConstraint, PrincipalConstraint, ResourceConstraint};

    use super::*;

    #[test]
    fn sanity_check() {
        let empty_annotations: Arc<Annotations> = Arc::default();
        let one_plus_two = Arc::new(Expr::add(Expr::val(1), Expr::val(2)));
        let three_plus_four = Arc::new(Expr::add(Expr::val(3), Expr::val(4)));
        let a = once((PolicyID::from_string("a"), empty_annotations.clone()));
        let bc = [
            (
                PolicyID::from_string("b"),
                (ErrorState::Error, empty_annotations.clone()),
            ),
            (
                PolicyID::from_string("c"),
                (ErrorState::NoError, empty_annotations.clone()),
            ),
        ];
        let d = once((
            PolicyID::from_string("d"),
            (one_plus_two.clone(), empty_annotations.clone()),
        ));
        let e = once((PolicyID::from_string("e"), empty_annotations.clone()));
        let fg = [
            (
                PolicyID::from_string("f"),
                (ErrorState::Error, empty_annotations.clone()),
            ),
            (
                PolicyID::from_string("g"),
                (ErrorState::NoError, empty_annotations.clone()),
            ),
        ];
        let h = once((
            PolicyID::from_string("h"),
            (three_plus_four.clone(), empty_annotations.clone()),
        ));
        let errs = empty();
        let pr = PartialResponse::new(a, bc, d, e, fg, h, errs);
        assert_eq!(
            pr.definitely_satisfied_permits().collect::<HashSet<_>>(),
            HashSet::from([&PolicyID::from_string("a")])
        );
        assert_eq!(
            pr.definitely_satisfied_forbids().collect::<HashSet<_>>(),
            HashSet::from([&PolicyID::from_string("e")])
        );
        assert_eq!(
            pr.definitely_satisfied().collect::<HashSet<_>>(),
            HashSet::from([&PolicyID::from_string("a"), &PolicyID::from_string("e")])
        );
        assert_eq!(
            pr.definitely_errored().collect::<HashSet<_>>(),
            HashSet::from([&PolicyID::from_string("b"), &PolicyID::from_string("f")])
        );
        assert_eq!(
            pr.may_be_determining().collect::<HashSet<_>>(),
            HashSet::from([&PolicyID::from_string("e"), &PolicyID::from_string("h")])
        );
        assert_eq!(
            pr.must_be_determining().collect::<HashSet<_>>(),
            HashSet::from([&PolicyID::from_string("e")])
        );
        assert_eq!(pr.nontrivial_residuals().count(), 2);

        assert_eq!(
            pr.all_residuals().collect::<SlowSet<_>>(),
            SlowSet::from([
                Policy::from_when_clause(
                    Effect::Permit,
                    Expr::val(true),
                    PolicyID::from_string("a")
                ),
                Policy::from_when_clause(
                    Effect::Permit,
                    Expr::val(false),
                    PolicyID::from_string("b")
                ),
                Policy::from_when_clause(
                    Effect::Permit,
                    Expr::val(false),
                    PolicyID::from_string("c")
                ),
                Policy::from_when_clause_annos(
                    Effect::Permit,
                    one_plus_two.clone(),
                    PolicyID::from_string("d"),
                    Arc::default()
                ),
                Policy::from_when_clause(
                    Effect::Forbid,
                    Expr::val(true),
                    PolicyID::from_string("e")
                ),
                Policy::from_when_clause(
                    Effect::Forbid,
                    Expr::val(false),
                    PolicyID::from_string("f")
                ),
                Policy::from_when_clause(
                    Effect::Forbid,
                    Expr::val(false),
                    PolicyID::from_string("g")
                ),
                Policy::from_when_clause_annos(
                    Effect::Forbid,
                    three_plus_four.clone(),
                    PolicyID::from_string("h"),
                    Arc::default()
                ),
            ])
        );
        assert_eq!(
            pr.nontrivial_residual_ids().collect::<HashSet<_>>(),
            HashSet::from([&PolicyID::from_string("d"), &PolicyID::from_string("h")])
        );

        assert_eq!(
            pr.nontrivial_residuals().collect::<SlowSet<_>>(),
            SlowSet::from([
                Policy::from_when_clause_annos(
                    Effect::Permit,
                    one_plus_two.clone(),
                    PolicyID::from_string("d"),
                    Arc::default()
                ),
                Policy::from_when_clause_annos(
                    Effect::Forbid,
                    three_plus_four.clone(),
                    PolicyID::from_string("h"),
                    Arc::default()
                ),
            ])
        );

        assert_eq!(pr.get(&PolicyID::from_string("a")), Some(&Expr::val(true)));
        assert_eq!(pr.get(&PolicyID::from_string("b")), Some(&Expr::val(false)));
        assert_eq!(pr.get(&PolicyID::from_string("c")), Some(&Expr::val(false)));
        assert_eq!(
            pr.get(&PolicyID::from_string("d")),
            Some(&Expr::add(Expr::val(1), Expr::val(2)))
        );
        assert_eq!(pr.get(&PolicyID::from_string("e")), Some(&Expr::val(true)));
        assert_eq!(pr.get(&PolicyID::from_string("f")), Some(&Expr::val(false)));
        assert_eq!(pr.get(&PolicyID::from_string("g")), Some(&Expr::val(false)));
        assert_eq!(
            pr.get(&PolicyID::from_string("h")),
            Some(&Expr::add(Expr::val(3), Expr::val(4)))
        );
    }

    #[test]
    fn build_policies_trivial_permit() {
        let e = Arc::new(Expr::add(Expr::val(1), Expr::val(2)));
        let id = PolicyID::from_string("foo");
        let p = construct_policy((Effect::Permit, &id, &e, &Arc::default()));
        assert_eq!(p.effect(), Effect::Permit);
        assert!(p.annotations().next().is_none());
        assert_eq!(p.action_constraint(), &ActionConstraint::Any);
        assert_eq!(p.principal_constraint(), PrincipalConstraint::any());
        assert_eq!(p.resource_constraint(), ResourceConstraint::any());
        assert_eq!(p.id(), &id);
        assert_eq!(p.non_head_constraints(), e.as_ref());
    }

    #[test]
    fn build_policies_trivial_forbid() {
        let e = Arc::new(Expr::add(Expr::val(1), Expr::val(2)));
        let id = PolicyID::from_string("foo");
        let p = construct_policy((Effect::Forbid, &id, &e, &Arc::default()));
        assert_eq!(p.effect(), Effect::Forbid);
        assert!(p.annotations().next().is_none());
        assert_eq!(p.action_constraint(), &ActionConstraint::Any);
        assert_eq!(p.principal_constraint(), PrincipalConstraint::any());
        assert_eq!(p.resource_constraint(), ResourceConstraint::any());
        assert_eq!(p.id(), &id);
        assert_eq!(p.non_head_constraints(), e.as_ref());
    }

    #[test]
    fn did_error_error() {
        assert_eq!(
            did_error((
                &PolicyID::from_string("foo"),
                &(ErrorState::Error, Arc::default())
            )),
            Some(&PolicyID::from_string("foo"))
        );
    }

    #[test]
    fn did_error_noerror() {
        assert_eq!(
            did_error((
                &PolicyID::from_string("foo"),
                &(ErrorState::NoError, Arc::default())
            )),
            None,
        );
    }
}
