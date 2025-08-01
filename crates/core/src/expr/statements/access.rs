use crate::ctx::Context;
use crate::dbs::Options;
use crate::doc::CursorDoc;
use crate::err::Error;
use crate::expr::access_type::BearerAccessSubject;
use crate::expr::{
	AccessType, Array, Base, Cond, Datetime, Duration, FlowResultExt as _, Ident, Object, Strand,
	Thing, Uuid, Value,
};
use crate::iam::{Action, ResourceKind};
use crate::kvs::impl_kv_value_revisioned;
use anyhow::{Result, bail, ensure};
use md5::Digest;
use rand::Rng;
use reblessive::tree::Stk;
use revision::revisioned;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::fmt;
use std::fmt::{Display, Formatter};

// Keys and their identifiers are generated randomly from a 62-character pool.
pub static GRANT_BEARER_CHARACTER_POOL: &[u8] =
	b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
// The key identifier should not have collisions to prevent confusion.
// However, collisions should be handled gracefully when issuing grants.
// The first character of the key identifier will not be a digit to prevent parsing issues.
// With 12 characters from the pool, one alphabetic, the key identifier part has ~68 bits of entropy.
pub static GRANT_BEARER_ID_LENGTH: usize = 12;
// With 24 characters from the pool, the key part has ~140 bits of entropy.
pub static GRANT_BEARER_KEY_LENGTH: usize = 24;

#[revisioned(revision = 1)]
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub enum AccessStatement {
	Grant(AccessStatementGrant),   // Create access grant.
	Show(AccessStatementShow),     // Show access grants.
	Revoke(AccessStatementRevoke), // Revoke access grant.
	Purge(AccessStatementPurge),   // Purge access grants.
}

#[revisioned(revision = 1)]
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub struct AccessStatementGrant {
	pub ac: Ident,
	pub base: Option<Base>,
	pub subject: Subject,
}

#[revisioned(revision = 1)]
#[derive(Clone, Debug, Default, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub struct AccessStatementShow {
	pub ac: Ident,
	pub base: Option<Base>,
	pub gr: Option<Ident>,
	pub cond: Option<Cond>,
}

#[revisioned(revision = 1)]
#[derive(Clone, Debug, Default, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub struct AccessStatementRevoke {
	pub ac: Ident,
	pub base: Option<Base>,
	pub gr: Option<Ident>,
	pub cond: Option<Cond>,
}

#[revisioned(revision = 1)]
#[derive(Clone, Debug, Default, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub struct AccessStatementPurge {
	pub ac: Ident,
	pub base: Option<Base>,
	pub expired: bool,
	pub revoked: bool,
	pub grace: Duration,
}

#[revisioned(revision = 1)]
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub struct AccessGrant {
	pub id: Ident,                    // Unique grant identifier.
	pub ac: Ident,                    // Access method used to create the grant.
	pub creation: Datetime,           // Grant creation time.
	pub expiration: Option<Datetime>, // Grant expiration time, if any.
	pub revocation: Option<Datetime>, // Grant revocation time, if any.
	pub subject: Subject,             // Subject of the grant.
	pub grant: Grant,                 // Grant data.
}
impl_kv_value_revisioned!(AccessGrant);

impl AccessGrant {
	/// Returns a version of the statement where potential secrets are redacted.
	/// This function should be used when displaying the statement to datastore users.
	/// This function should NOT be used when displaying the statement for export purposes.
	pub fn redacted(&self) -> AccessGrant {
		let mut ags = self.clone();
		ags.grant = match ags.grant {
			Grant::Jwt(mut gr) => {
				// Token should not even be stored. We clear it just as a precaution.
				gr.token = None;
				Grant::Jwt(gr)
			}
			Grant::Record(mut gr) => {
				// Token should not even be stored. We clear it just as a precaution.
				gr.token = None;
				Grant::Record(gr)
			}
			Grant::Bearer(mut gr) => {
				// Key is stored, but should not usually be displayed.
				gr.key = "[REDACTED]".into();
				Grant::Bearer(gr)
			}
		};
		ags
	}

	// Returns if the access grant is expired.
	pub fn is_expired(&self) -> bool {
		match &self.expiration {
			Some(exp) => exp < &Datetime::default(),
			None => false,
		}
	}

	// Returns if the access grant is revoked.
	pub fn is_revoked(&self) -> bool {
		self.revocation.is_some()
	}

	// Returns if the access grant is active.
	pub fn is_active(&self) -> bool {
		!(self.is_expired() || self.is_revoked())
	}
}

impl From<AccessGrant> for Object {
	fn from(grant: AccessGrant) -> Self {
		let mut res = Object::default();
		res.insert("id".to_owned(), Value::from(grant.id.to_raw()));
		res.insert("ac".to_owned(), Value::from(grant.ac.to_raw()));
		res.insert("type".to_owned(), Value::from(grant.grant.variant()));
		res.insert("creation".to_owned(), Value::from(grant.creation));
		res.insert("expiration".to_owned(), Value::from(grant.expiration));
		res.insert("revocation".to_owned(), Value::from(grant.revocation));
		let mut sub = Object::default();
		match grant.subject {
			Subject::Record(id) => sub.insert("record".to_owned(), Value::from(id)),
			Subject::User(name) => sub.insert("user".to_owned(), Value::from(name.to_raw())),
		};
		res.insert("subject".to_owned(), Value::from(sub));

		let mut gr = Object::default();
		match grant.grant {
			Grant::Jwt(jg) => {
				gr.insert("jti".to_owned(), Value::from(jg.jti));
				if let Some(token) = jg.token {
					gr.insert("token".to_owned(), Value::from(token));
				}
			}
			Grant::Record(rg) => {
				gr.insert("rid".to_owned(), Value::from(rg.rid));
				gr.insert("jti".to_owned(), Value::from(rg.jti));
				if let Some(token) = rg.token {
					gr.insert("token".to_owned(), Value::from(token));
				}
			}
			Grant::Bearer(bg) => {
				gr.insert("id".to_owned(), Value::from(bg.id.to_raw()));
				gr.insert("key".to_owned(), Value::from(bg.key));
			}
		};
		res.insert("grant".to_owned(), Value::from(gr));

		res
	}
}

#[revisioned(revision = 1)]
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub enum Subject {
	Record(Thing),
	User(Ident),
}

impl Subject {
	// Returns the main identifier of a subject as a string.
	pub fn id(&self) -> String {
		match self {
			Subject::Record(id) => id.to_raw(),
			Subject::User(name) => name.to_raw(),
		}
	}
}

#[revisioned(revision = 1)]
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub enum Grant {
	Jwt(GrantJwt),
	Record(GrantRecord),
	Bearer(GrantBearer),
}

impl Grant {
	// Returns the type of the grant as a string.
	pub fn variant(&self) -> &str {
		match self {
			Grant::Jwt(_) => "jwt",
			Grant::Record(_) => "record",
			Grant::Bearer(_) => "bearer",
		}
	}
}

#[revisioned(revision = 1)]
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub struct GrantJwt {
	pub jti: Uuid,             // JWT ID
	pub token: Option<Strand>, // JWT. Will not be stored after being returned.
}

#[revisioned(revision = 1)]
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub struct GrantRecord {
	pub rid: Uuid,             // Record ID
	pub jti: Uuid,             // JWT ID
	pub token: Option<Strand>, // JWT. Will not be stored after being returned.
}

#[revisioned(revision = 1)]
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub struct GrantBearer {
	pub id: Ident, // Key ID
	// Key. Will not be stored and be returned as redacted.
	// Immediately after generation, it will contain the plaintext key.
	// Will be hashed before storage so that the plaintext key is not stored.
	pub key: Strand,
}

impl GrantBearer {
	pub fn new(prefix: &str) -> Self {
		let id = format!(
			"{}{}",
			// The pool for the first character of the key identifier excludes digits.
			random_string(1, &GRANT_BEARER_CHARACTER_POOL[10..]),
			random_string(GRANT_BEARER_ID_LENGTH - 1, GRANT_BEARER_CHARACTER_POOL)
		);
		let secret = random_string(GRANT_BEARER_KEY_LENGTH, GRANT_BEARER_CHARACTER_POOL);
		Self {
			id: id.clone().into(),
			key: format!("{prefix}-{id}-{secret}").into(),
		}
	}

	pub fn hashed(self) -> Self {
		// The hash of the bearer key is stored to mitigate the impact of a read-only compromise.
		// We use SHA-256 as the key needs to be verified performantly for every operation.
		// Unlike with passwords, brute force and rainbow tables are infeasable due to the key length.
		// When hashing the bearer keys, the prefix and key identifier are kept as salt.
		let mut hasher = Sha256::new();
		hasher.update(self.key.as_string());
		let hash = hasher.finalize();
		let hash_hex = format!("{hash:x}").into();

		Self {
			key: hash_hex,
			..self
		}
	}
}

fn random_string(length: usize, pool: &[u8]) -> String {
	let mut rng = rand::thread_rng();
	let string: String = (0..length)
		.map(|_| {
			let i = rng.gen_range(0..pool.len());
			pool[i] as char
		})
		.collect();
	string
}

pub async fn create_grant(
	stmt: &AccessStatementGrant,
	ctx: &Context,
	opt: &Options,
) -> Result<AccessGrant> {
	let base = match &stmt.base {
		Some(base) => base.clone(),
		None => opt.selected_base()?,
	};
	// Allowed to run?
	opt.is_allowed(Action::Edit, ResourceKind::Access, &base)?;
	// Get the transaction.
	let txn = ctx.tx();
	// Clear the cache.
	txn.clear();
	// Read the access definition.
	let ac = match base {
		Base::Root => txn.get_root_access(&stmt.ac).await?,
		Base::Ns => txn.get_ns_access(opt.ns()?, &stmt.ac).await?,
		Base::Db => {
			let (ns, db) = opt.ns_db()?;
			txn.get_db_access(ns, db, &stmt.ac).await?
		}
		_ => {
			bail!(Error::Unimplemented(
				"Managing access methods outside of root, namespace and database levels"
					.to_string(),
			))
		}
	};
	// Verify the access type.
	match &ac.kind {
		AccessType::Jwt(_) => {
			Err(anyhow::Error::new(Error::Unimplemented(format!("Grants for JWT on {base}"))))
		}
		AccessType::Record(at) => {
			match &stmt.subject {
				Subject::User(_) => {
					bail!(Error::AccessGrantInvalidSubject);
				}
				Subject::Record(_) => {
					// If the grant is being created for a record, a database must be selected.
					ensure!(matches!(base, Base::Db), Error::DbEmpty);
				}
			};
			// The record access type must allow issuing bearer grants.
			let atb = match &at.bearer {
				Some(bearer) => bearer,
				None => bail!(Error::AccessMethodMismatch),
			};
			// Create a new bearer key.
			let grant = GrantBearer::new(atb.kind.prefix());
			let gr = AccessGrant {
				ac: ac.name.clone(),
				// Unique grant identifier.
				// In the case of bearer grants, the key identifier.
				id: grant.id.clone(),
				// Current time.
				creation: Datetime::default(),
				// Current time plus grant duration. Only if set.
				expiration: ac.duration.grant.map(|d| d + Datetime::default()),
				// The grant is initially not revoked.
				revocation: None,
				// Subject associated with the grant.
				subject: stmt.subject.clone(),
				// The contents of the grant.
				grant: Grant::Bearer(grant.clone()),
			};

			// Create the grant.
			// On the very unlikely event of a collision, "put" will return an error.
			let res = match base {
				Base::Db => {
					// Create a hashed version of the grant for storage.
					let mut gr_store = gr.clone();
					gr_store.grant = Grant::Bearer(grant.hashed());
					let (ns, db) = opt.ns_db()?;
					let key = crate::key::database::access::gr::new(ns, db, &gr.ac, &gr.id);
					txn.get_or_add_ns(ns, opt.strict).await?;
					txn.get_or_add_db(ns, db, opt.strict).await?;
					txn.put(&key, &gr_store, None).await
				}
				_ => bail!(Error::AccessLevelMismatch),
			};

			// Check if a collision was found in order to log a specific error on the server.
			// For an access method with a billion grants, this chance is of only one in 295 billion.
			match res {
				Ok(_) => {}
				Err(e) => {
					if matches!(e.downcast_ref(), Some(Error::TxKeyAlreadyExists)) {
						error!(
							"A collision was found when attempting to create a new grant. Purging inactive grants is advised"
						)
					}
					return Err(e);
				}
			}

			info!(
				"Access method '{}' was used to create grant '{}' of type '{}' for '{}' by '{}'",
				gr.ac,
				gr.id,
				gr.grant.variant(),
				gr.subject.id(),
				opt.auth.id()
			);

			// Return the original version of the grant.
			// This is the only time the the plaintext key is returned.
			Ok(gr)
		}
		AccessType::Bearer(at) => {
			match &stmt.subject {
				Subject::User(user) => {
					// Grant subject must match access method subject.
					ensure!(
						matches!(&at.subject, BearerAccessSubject::User),
						Error::AccessGrantInvalidSubject
					);
					// If the grant is being created for a user, the user must exist.
					match base {
						Base::Root => txn.get_root_user(user).await?,
						Base::Ns => txn.get_ns_user(opt.ns()?, user).await?,
						Base::Db => {
							let (ns, db) = opt.ns_db()?;
							txn.get_db_user(ns, db, user).await?
						}
						_ => bail!(Error::Unimplemented(
							"Managing access methods outside of root, namespace and database levels".to_string(),
						)),
					};
				}
				Subject::Record(_) => {
					// If the grant is being created for a record, a database must be selected.
					ensure!(matches!(base, Base::Db), Error::DbEmpty);
					// Grant subject must match access method subject.
					ensure!(
						matches!(&at.subject, BearerAccessSubject::Record),
						Error::AccessGrantInvalidSubject
					);
					// A grant can be created for a record that does not exist yet.
				}
			};
			// Create a new bearer key.
			let grant = GrantBearer::new(at.kind.prefix());
			let gr = AccessGrant {
				ac: ac.name.clone(),
				// Unique grant identifier.
				// In the case of bearer grants, the key identifier.
				id: grant.id.clone(),
				// Current time.
				creation: Datetime::default(),
				// Current time plus grant duration. Only if set.
				expiration: ac.duration.grant.map(|d| d + Datetime::default()),
				// The grant is initially not revoked.
				revocation: None,
				// Subject associated with the grant.
				subject: stmt.subject.clone(),
				// The contents of the grant.
				grant: Grant::Bearer(grant.clone()),
			};

			// Create the grant.
			// On the very unlikely event of a collision, "put" will return an error.
			// Create a hashed version of the grant for storage.
			let mut gr_store = gr.clone();
			gr_store.grant = Grant::Bearer(grant.hashed());
			let res = match base {
				Base::Root => {
					let key = crate::key::root::access::gr::new(&gr.ac, &gr.id);
					txn.put(&key, &gr_store, None).await
				}
				Base::Ns => {
					let key = crate::key::namespace::access::gr::new(opt.ns()?, &gr.ac, &gr.id);
					txn.get_or_add_ns(opt.ns()?, opt.strict).await?;
					txn.put(&key, &gr_store, None).await
				}
				Base::Db => {
					let (ns, db) = opt.ns_db()?;
					let key = crate::key::database::access::gr::new(ns, db, &gr.ac, &gr.id);
					txn.get_or_add_ns(ns, opt.strict).await?;
					txn.get_or_add_db(ns, db, opt.strict).await?;
					txn.put(&key, &gr_store, None).await
				}
				_ => bail!(Error::Unimplemented(
					"Managing access methods outside of root, namespace and database levels"
						.to_string(),
				)),
			};

			// Check if a collision was found in order to log a specific error on the server.
			// For an access method with a billion grants, this chance is of only one in 295 billion.
			match res {
				Ok(_) => {}
				Err(e) => {
					if matches!(e.downcast_ref(), Some(Error::TxKeyAlreadyExists)) {
						error!(
							"A collision was found when attempting to create a new grant. Purging inactive grants is advised"
						)
					}
					return Err(e);
				}
			}

			info!(
				"Access method '{}' was used to create grant '{}' of type '{}' for '{}' by '{}'",
				gr.ac,
				gr.id,
				gr.grant.variant(),
				gr.subject.id(),
				opt.auth.id()
			);

			// Return the original version of the grant.
			// This is the only time the the plaintext key is returned.
			Ok(gr)
		}
	}
}

async fn compute_grant(
	stmt: &AccessStatementGrant,
	ctx: &Context,
	opt: &Options,
	_doc: Option<&CursorDoc>,
) -> Result<Value> {
	let grant = create_grant(stmt, ctx, opt).await?;
	Ok(Value::Object(grant.into()))
}

async fn compute_show(
	stmt: &AccessStatementShow,
	stk: &mut Stk,
	ctx: &Context,
	opt: &Options,
	_doc: Option<&CursorDoc>,
) -> Result<Value> {
	let base = match &stmt.base {
		Some(base) => base.clone(),
		None => opt.selected_base()?,
	};
	// Allowed to run?
	opt.is_allowed(Action::View, ResourceKind::Access, &base)?;
	// Get the transaction.
	let txn = ctx.tx();
	// Clear the cache.
	txn.clear();
	// Check if the access method exists.
	match base {
		Base::Root => txn.get_root_access(&stmt.ac).await?,
		Base::Ns => txn.get_ns_access(opt.ns()?, &stmt.ac).await?,
		Base::Db => {
			let (ns, db) = opt.ns_db()?;
			txn.get_db_access(ns, db, &stmt.ac).await?
		}
		_ => {
			bail!(Error::Unimplemented(
				"Managing access methods outside of root, namespace and database levels"
					.to_string(),
			))
		}
	};

	// Get the grants to show.
	match &stmt.gr {
		Some(gr) => {
			let grant = match base {
				Base::Root => (*txn.get_root_access_grant(&stmt.ac, gr).await?).clone(),
				Base::Ns => (*txn.get_ns_access_grant(opt.ns()?, &stmt.ac, gr).await?).clone(),
				Base::Db => {
					let (ns, db) = opt.ns_db()?;
					(*txn.get_db_access_grant(ns, db, &stmt.ac, gr).await?).clone()
				}
				_ => bail!(Error::Unimplemented(
					"Managing access methods outside of root, namespace and database levels"
						.to_string(),
				)),
			};

			Ok(Value::Object(grant.redacted().into()))
		}
		None => {
			// Get all grants.
			let grs = match base {
				Base::Root => txn.all_root_access_grants(&stmt.ac).await?,
				Base::Ns => txn.all_ns_access_grants(opt.ns()?, &stmt.ac).await?,
				Base::Db => {
					let (ns, db) = opt.ns_db()?;
					txn.all_db_access_grants(ns, db, &stmt.ac).await?
				}
				_ => bail!(Error::Unimplemented(
					"Managing access methods outside of root, namespace and database levels"
						.to_string(),
				)),
			};

			let mut show = Vec::new();
			for gr in grs.iter() {
				// If provided, check if grant matches conditions.
				if let Some(cond) = &stmt.cond {
					// Redact grant before evaluating conditions.
					let redacted_gr = Value::Object(gr.redacted().clone().into());
					if !cond
						.compute(
							stk,
							ctx,
							opt,
							Some(&CursorDoc {
								rid: None,
								ir: None,
								doc: redacted_gr.into(),
							}),
						)
						.await
						.catch_return()?
						.is_truthy()
					{
						// Skip grant if it does not match the provided conditions.
						continue;
					}
				}

				// Store revoked version of the redacted grant.
				show.push(Value::Object(gr.redacted().clone().into()));
			}

			Ok(Value::Array(show.into()))
		}
	}
}

pub async fn revoke_grant(
	stmt: &AccessStatementRevoke,
	stk: &mut Stk,
	ctx: &Context,
	opt: &Options,
) -> Result<Value> {
	let base = match &stmt.base {
		Some(base) => base.clone(),
		None => opt.selected_base()?,
	};
	// Allowed to run?
	opt.is_allowed(Action::Edit, ResourceKind::Access, &base)?;
	// Get the transaction
	let txn = ctx.tx();
	// Clear the cache
	txn.clear();
	// Check if the access method exists.
	match base {
		Base::Root => txn.get_root_access(&stmt.ac).await?,
		Base::Ns => txn.get_ns_access(opt.ns()?, &stmt.ac).await?,
		Base::Db => {
			let (ns, db) = opt.ns_db()?;
			txn.get_db_access(ns, db, &stmt.ac).await?
		}
		_ => {
			bail!(Error::Unimplemented(
				"Managing access methods outside of root, namespace and database levels"
					.to_string(),
			))
		}
	};

	// Get the grants to revoke.
	let mut revoked = Vec::new();
	match &stmt.gr {
		Some(gr) => {
			let mut revoke = match base {
				Base::Root => (*txn.get_root_access_grant(&stmt.ac, gr).await?).clone(),
				Base::Ns => (*txn.get_ns_access_grant(opt.ns()?, &stmt.ac, gr).await?).clone(),
				Base::Db => {
					let (ns, db) = opt.ns_db()?;
					(*txn.get_db_access_grant(ns, db, &stmt.ac, gr).await?).clone()
				}
				_ => bail!(Error::Unimplemented(
					"Managing access methods outside of root, namespace and database levels"
						.to_string(),
				)),
			};
			ensure!(revoke.revocation.is_none(), Error::AccessGrantRevoked);
			revoke.revocation = Some(Datetime::default());

			// Revoke the grant.
			match base {
				Base::Root => {
					let key = crate::key::root::access::gr::new(&stmt.ac, gr);
					txn.set(&key, &revoke, None).await?;
				}
				Base::Ns => {
					let key = crate::key::namespace::access::gr::new(opt.ns()?, &stmt.ac, gr);
					txn.get_or_add_ns(opt.ns()?, opt.strict).await?;
					txn.set(&key, &revoke, None).await?;
				}
				Base::Db => {
					let (ns, db) = opt.ns_db()?;
					let key = crate::key::database::access::gr::new(ns, db, &stmt.ac, gr);
					txn.get_or_add_ns(ns, opt.strict).await?;
					txn.get_or_add_db(ns, db, opt.strict).await?;
					txn.set(&key, &revoke, None).await?;
				}
				_ => {
					bail!(Error::Unimplemented(
						"Managing access methods outside of root, namespace and database levels"
							.to_string(),
					))
				}
			};

			info!(
				"Access method '{}' was used to revoke grant '{}' of type '{}' for '{}' by '{}'",
				revoke.ac,
				revoke.id,
				revoke.grant.variant(),
				revoke.subject.id(),
				opt.auth.id()
			);

			revoked.push(Value::Object(revoke.redacted().into()));
		}
		None => {
			// Get all grants.
			let grs = match base {
				Base::Root => txn.all_root_access_grants(&stmt.ac).await?,
				Base::Ns => txn.all_ns_access_grants(opt.ns()?, &stmt.ac).await?,
				Base::Db => {
					let (ns, db) = opt.ns_db()?;
					txn.all_db_access_grants(ns, db, &stmt.ac).await?
				}
				_ => bail!(Error::Unimplemented(
					"Managing access methods outside of root, namespace and database levels"
						.to_string(),
				)),
			};

			for gr in grs.iter() {
				// If the grant is already revoked, it cannot be revoked again.
				if gr.revocation.is_some() {
					continue;
				}

				// If provided, check if grant matches conditions.
				if let Some(cond) = &stmt.cond {
					// Redact grant before evaluating conditions.
					let redacted_gr = Value::Object(gr.redacted().clone().into());
					if !cond
						.compute(
							stk,
							ctx,
							opt,
							Some(&CursorDoc {
								rid: None,
								ir: None,
								doc: redacted_gr.into(),
							}),
						)
						.await
						.catch_return()?
						.is_truthy()
					{
						// Skip grant if it does not match the provided conditions.
						continue;
					}
				}

				let mut gr = gr.clone();
				gr.revocation = Some(Datetime::default());

				// Revoke the grant.
				match base {
					Base::Root => {
						let key = crate::key::root::access::gr::new(&stmt.ac, &gr.id);
						txn.set(&key, &gr, None).await?;
					}
					Base::Ns => {
						let key =
							crate::key::namespace::access::gr::new(opt.ns()?, &stmt.ac, &gr.id);
						txn.get_or_add_ns(opt.ns()?, opt.strict).await?;
						txn.set(&key, &gr, None).await?;
					}
					Base::Db => {
						let (ns, db) = opt.ns_db()?;
						let key = crate::key::database::access::gr::new(ns, db, &stmt.ac, &gr.id);
						txn.get_or_add_ns(ns, opt.strict).await?;
						txn.get_or_add_db(ns, db, opt.strict).await?;
						txn.set(&key, &gr, None).await?;
					}
					_ => bail!(Error::Unimplemented(
						"Managing access methods outside of root, namespace and database levels"
							.to_string(),
					)),
				};

				info!(
					"Access method '{}' was used to revoke grant '{}' of type '{}' for '{}' by '{}'",
					gr.ac,
					gr.id,
					gr.grant.variant(),
					gr.subject.id(),
					opt.auth.id()
				);

				// Store revoked version of the redacted grant.
				revoked.push(Value::Object(gr.redacted().into()));
			}
		}
	}

	// Return revoked grants.
	Ok(Value::Array(revoked.into()))
}

async fn compute_revoke(
	stmt: &AccessStatementRevoke,
	stk: &mut Stk,
	ctx: &Context,
	opt: &Options,
	_doc: Option<&CursorDoc>,
) -> Result<Value> {
	let revoked = revoke_grant(stmt, stk, ctx, opt).await?;
	Ok(Value::Array(revoked.into()))
}

async fn compute_purge(
	stmt: &AccessStatementPurge,
	ctx: &Context,
	opt: &Options,
	_doc: Option<&CursorDoc>,
) -> Result<Value> {
	let base = match &stmt.base {
		Some(base) => base.clone(),
		None => opt.selected_base()?,
	};
	// Allowed to run?
	opt.is_allowed(Action::Edit, ResourceKind::Access, &base)?;
	// Get the transaction.
	let txn = ctx.tx();
	// Clear the cache.
	txn.clear();
	// Check if the access method exists.
	match base {
		Base::Root => txn.get_root_access(&stmt.ac).await?,
		Base::Ns => txn.get_ns_access(opt.ns()?, &stmt.ac).await?,
		Base::Db => {
			let (ns, db) = opt.ns_db()?;
			txn.get_db_access(ns, db, &stmt.ac).await?
		}
		_ => {
			bail!(Error::Unimplemented(
				"Managing access methods outside of root, namespace and database levels"
					.to_string(),
			))
		}
	};
	// Get all grants to purge.
	let mut purged = Array::default();
	let grs = match base {
		Base::Root => txn.all_root_access_grants(&stmt.ac).await?,
		Base::Ns => txn.all_ns_access_grants(opt.ns()?, &stmt.ac).await?,
		Base::Db => {
			let (ns, db) = opt.ns_db()?;
			txn.all_db_access_grants(ns, db, &stmt.ac).await?
		}
		_ => {
			bail!(Error::Unimplemented(
				"Managing access methods outside of root, namespace and database levels"
					.to_string(),
			))
		}
	};
	for gr in grs.iter() {
		// Determine if the grant should purged based on expiration or revocation.
		let now = Datetime::default();
		// We can convert to unsigned integer as substraction is saturating.
		// Revocation times should never exceed the current time.
		// Grants expired or revoked at a future time will not be purged.
		// Grants expired or revoked at exactly the current second will not be purged.
		let purge_expired = stmt.expired
			&& gr.expiration.as_ref().is_some_and(|exp| {
				                 now.timestamp() >= exp.timestamp() // Prevent saturating when not expired yet.
				                     && (now.timestamp().saturating_sub(exp.timestamp()) as u64) > stmt.grace.secs()
				             });
		let purge_revoked = stmt.revoked
			&& gr.revocation.as_ref().is_some_and(|rev| {
				                 now.timestamp() >= rev.timestamp() // Prevent saturating when not revoked yet.
				                     && (now.timestamp().saturating_sub(rev.timestamp()) as u64) > stmt.grace.secs()
				             });
		// If it should, delete the grant and append the redacted version to the result.
		if purge_expired || purge_revoked {
			match base {
				Base::Root => txn.del(&crate::key::root::access::gr::new(&stmt.ac, &gr.id)).await?,
				Base::Ns => {
					txn.del(&crate::key::namespace::access::gr::new(opt.ns()?, &stmt.ac, &gr.id))
						.await?
				}
				Base::Db => {
					let (ns, db) = opt.ns_db()?;
					txn.del(&crate::key::database::access::gr::new(ns, db, &stmt.ac, &gr.id))
						.await?
				}
				_ => {
					bail!(Error::Unimplemented(
						"Managing access methods outside of root, namespace and database levels"
							.to_string(),
					))
				}
			};

			info!(
				"Access method '{}' was used to purge grant '{}' of type '{}' for '{}' by '{}'",
				gr.ac,
				gr.id,
				gr.grant.variant(),
				gr.subject.id(),
				opt.auth.id()
			);

			purged = purged + Value::Object(gr.redacted().clone().into());
		}
	}

	Ok(Value::Array(purged))
}

impl AccessStatement {
	/// Process this type returning a computed simple Value
	pub(crate) async fn compute(
		&self,
		stk: &mut Stk,
		ctx: &Context,
		opt: &Options,
		_doc: Option<&CursorDoc>,
	) -> Result<Value> {
		match self {
			AccessStatement::Grant(stmt) => compute_grant(stmt, ctx, opt, _doc).await,
			AccessStatement::Show(stmt) => compute_show(stmt, stk, ctx, opt, _doc).await,
			AccessStatement::Revoke(stmt) => compute_revoke(stmt, stk, ctx, opt, _doc).await,
			AccessStatement::Purge(stmt) => compute_purge(stmt, ctx, opt, _doc).await,
		}
	}
}

impl Display for AccessStatement {
	fn fmt(&self, f: &mut Formatter) -> fmt::Result {
		match self {
			Self::Grant(stmt) => {
				write!(f, "ACCESS {}", stmt.ac)?;
				if let Some(ref v) = stmt.base {
					write!(f, " ON {v}")?;
				}
				write!(f, " GRANT")?;
				match stmt.subject {
					Subject::User(_) => write!(f, " FOR USER {}", stmt.subject.id())?,
					Subject::Record(_) => write!(f, " FOR RECORD {}", stmt.subject.id())?,
				}
				Ok(())
			}
			Self::Show(stmt) => {
				write!(f, "ACCESS {}", stmt.ac)?;
				if let Some(ref v) = stmt.base {
					write!(f, " ON {v}")?;
				}
				write!(f, " SHOW")?;
				match &stmt.gr {
					Some(v) => write!(f, " GRANT {v}")?,
					None => match &stmt.cond {
						Some(v) => write!(f, " {v}")?,
						None => write!(f, " ALL")?,
					},
				};
				Ok(())
			}
			Self::Revoke(stmt) => {
				write!(f, "ACCESS {}", stmt.ac)?;
				if let Some(ref v) = stmt.base {
					write!(f, " ON {v}")?;
				}
				write!(f, " REVOKE")?;
				match &stmt.gr {
					Some(v) => write!(f, " GRANT {v}")?,
					None => match &stmt.cond {
						Some(v) => write!(f, " {v}")?,
						None => write!(f, " ALL")?,
					},
				};
				Ok(())
			}
			Self::Purge(stmt) => {
				write!(f, "ACCESS {}", stmt.ac)?;
				if let Some(ref v) = stmt.base {
					write!(f, " ON {v}")?;
				}
				write!(f, " PURGE")?;
				match (stmt.expired, stmt.revoked) {
					(true, false) => write!(f, " EXPIRED")?,
					(false, true) => write!(f, " REVOKED")?,
					(true, true) => write!(f, " EXPIRED, REVOKED")?,
					// This case should not parse.
					(false, false) => write!(f, " NONE")?,
				};
				if !stmt.grace.is_zero() {
					write!(f, " FOR {}", stmt.grace)?;
				}
				Ok(())
			}
		}
	}
}
