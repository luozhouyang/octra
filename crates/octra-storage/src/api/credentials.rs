//! Key-auth, basic-auth and JWT credential repositories.

use super::*;

impl ApiStore {
    /// Key-auth keys are sealed with a random nonce, so the plaintext UNIQUE
    /// constraint no longer catches duplicates. Compare decrypted values
    /// before writing to keep key-auth keys unique.
    async fn ensure_unique_keyauth_key(&self, id: &str, key: &str) -> Result<()> {
        let rows = sqlx::query("SELECT id, key FROM keyauth_credentials WHERE id <> ?")
            .bind(id)
            .fetch_all(self.pool())
            .await
            .map_err(|e| map_db_error(e, "credential"))?;
        for row in rows {
            let existing_id: String = decode(row.try_get("id"), "credential")?;
            let stored: String = decode(row.try_get("key"), "credential")?;
            let existing = open_secret(
                &self.secrets,
                &stored,
                "keyauth_credentials",
                &existing_id,
                "key",
            )?;
            if existing == key {
                return Err(Error::Storage(
                    "conflict: credential already exists".to_string(),
                ));
            }
        }
        Ok(())
    }

    // Key-auth credentials

    pub async fn list_credentials_page(
        &self,
        consumer_id: Option<&str>,
        page: &ListPage,
    ) -> Result<Page<KeyauthCredential>> {
        self.page_rows(
            "keyauth_credentials",
            KEYAUTH_COLUMNS,
            consumer_id.map(|id| ("consumer_id", id)),
            None,
            page,
            |row| map_keyauth(row, &self.secrets),
            |credential| (credential.created_at, credential.id.clone()),
        )
        .await
    }

    pub async fn list_credentials(
        &self,
        consumer_id: Option<&str>,
    ) -> Result<Vec<KeyauthCredential>> {
        let rows = sqlx::query(&format!(
            "SELECT {KEYAUTH_COLUMNS} FROM keyauth_credentials \
             WHERE (?1 IS NULL OR consumer_id = ?1) ORDER BY created_at ASC, id ASC"
        ))
        .bind(consumer_id)
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credentials"))?;
        rows.iter()
            .map(|row| map_keyauth(row, &self.secrets))
            .collect()
    }

    pub async fn get_credential(&self, id: &str) -> Result<Option<KeyauthCredential>> {
        let row = sqlx::query(&format!(
            "SELECT {KEYAUTH_COLUMNS} FROM keyauth_credentials WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;
        row.as_ref()
            .map(|row| map_keyauth(row, &self.secrets))
            .transpose()
    }

    pub async fn create_credential(
        &self,
        input: &CreateKeyauthCredential,
    ) -> Result<KeyauthCredential> {
        self.ensure_parent("consumers", &input.consumer_id, "consumer")
            .await?;
        let id = input.id.clone().unwrap_or_else(|| new_id("cred"));
        let key = input
            .key
            .clone()
            .unwrap_or_else(|| format!("key_{}", uuid::Uuid::new_v4().simple()));
        self.ensure_unique_keyauth_key(&id, &key).await?;
        let sealed_key = self.seal_secret(&key, "keyauth_credentials", &id, "key")?;
        sqlx::query(&format!(
            "INSERT INTO keyauth_credentials (id, consumer_id, key, ttl, tags, created_at) \
             VALUES (?, ?, ?, ?, ?, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.consumer_id)
        .bind(&sealed_key)
        .bind(input.ttl)
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;

        self.get_credential(&id)
            .await?
            .ok_or_else(|| Error::Storage("credential disappeared after insert".into()))
    }

    pub async fn update_credential(
        &self,
        id: &str,
        input: &UpdateKeyauthCredential,
    ) -> Result<KeyauthCredential> {
        if let Some(key) = input.key.as_deref() {
            self.ensure_unique_keyauth_key(id, key).await?;
        }
        let sealed_key = input
            .key
            .as_deref()
            .map(|value| self.seal_secret(value, "keyauth_credentials", id, "key"))
            .transpose()?;
        let result = sqlx::query(
            "UPDATE keyauth_credentials SET \
                key = COALESCE(?, key), \
                ttl = COALESCE(?, ttl), \
                tags = COALESCE(?, tags) \
             WHERE id = ?",
        )
        .bind(&sealed_key)
        .bind(input.ttl)
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;
        affected(result.rows_affected(), "credential")?;
        self.get_credential(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: credential not found".into()))
    }

    pub async fn delete_credential(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM keyauth_credentials WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "credential"))?;
        affected(result.rows_affected(), "credential")
    }

    // Basic-auth credentials

    pub async fn list_basicauth_page(
        &self,
        consumer_id: Option<&str>,
        page: &ListPage,
    ) -> Result<Page<BasicauthCredential>> {
        self.page_rows(
            "basicauth_credentials",
            BASICAUTH_COLUMNS,
            consumer_id.map(|id| ("consumer_id", id)),
            None,
            page,
            |row| map_basicauth(row, &self.secrets),
            |credential| (credential.created_at, credential.id.clone()),
        )
        .await
    }

    pub async fn list_basicauth(&self) -> Result<Vec<BasicauthCredential>> {
        let rows = sqlx::query(&format!(
            "SELECT {BASICAUTH_COLUMNS} FROM basicauth_credentials ORDER BY username ASC"
        ))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credentials"))?;
        rows.iter()
            .map(|row| map_basicauth(row, &self.secrets))
            .collect()
    }

    pub async fn create_basicauth(
        &self,
        input: &CreateBasicauthCredential,
    ) -> Result<BasicauthCredential> {
        self.ensure_parent("consumers", &input.consumer_id, "consumer")
            .await?;
        let id = input.id.clone().unwrap_or_else(|| new_id("basic"));
        let sealed_password =
            self.seal_secret(&input.password, "basicauth_credentials", &id, "password")?;
        sqlx::query(&format!(
            "INSERT INTO basicauth_credentials (id, consumer_id, username, password, tags, \
             created_at) VALUES (?, ?, ?, ?, ?, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.consumer_id)
        .bind(&input.username)
        .bind(&sealed_password)
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;
        self.get_basicauth(&id)
            .await?
            .ok_or_else(|| Error::Storage("credential disappeared after insert".into()))
    }

    pub async fn get_basicauth(&self, id: &str) -> Result<Option<BasicauthCredential>> {
        let row = sqlx::query(&format!(
            "SELECT {BASICAUTH_COLUMNS} FROM basicauth_credentials WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;
        row.as_ref()
            .map(|row| map_basicauth(row, &self.secrets))
            .transpose()
    }

    pub async fn update_basicauth(
        &self,
        id: &str,
        input: &UpdateBasicauthCredential,
    ) -> Result<BasicauthCredential> {
        let sealed_password = input
            .password
            .as_deref()
            .map(|value| self.seal_secret(value, "basicauth_credentials", id, "password"))
            .transpose()?;
        let result = sqlx::query(
            "UPDATE basicauth_credentials SET \
                username = COALESCE(?, username), \
                password = COALESCE(?, password), \
                tags = COALESCE(?, tags) \
             WHERE id = ?",
        )
        .bind(&input.username)
        .bind(&sealed_password)
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;
        affected(result.rows_affected(), "credential")?;
        self.get_basicauth(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: credential not found".into()))
    }

    pub async fn delete_basicauth(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM basicauth_credentials WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "credential"))?;
        affected(result.rows_affected(), "credential")
    }

    // JWT secrets

    pub async fn list_jwt_page(
        &self,
        consumer_id: Option<&str>,
        page: &ListPage,
    ) -> Result<Page<JwtSecret>> {
        self.page_rows(
            "jwt_secrets",
            JWT_COLUMNS,
            consumer_id.map(|id| ("consumer_id", id)),
            None,
            page,
            |row| map_jwt(row, &self.secrets),
            |credential| (credential.created_at, credential.id.clone()),
        )
        .await
    }

    pub async fn list_jwt(&self) -> Result<Vec<JwtSecret>> {
        let rows = sqlx::query(&format!(
            "SELECT {JWT_COLUMNS} FROM jwt_secrets ORDER BY created_at ASC, id ASC"
        ))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credentials"))?;
        rows.iter().map(|row| map_jwt(row, &self.secrets)).collect()
    }

    pub async fn create_jwt(&self, input: &CreateJwtSecret) -> Result<JwtSecret> {
        self.ensure_parent("consumers", &input.consumer_id, "consumer")
            .await?;
        let id = input.id.clone().unwrap_or_else(|| new_id("jwt"));
        let sealed_secret = self.seal_secret(&input.secret, "jwt_secrets", &id, "secret")?;
        sqlx::query(&format!(
            "INSERT INTO jwt_secrets (id, consumer_id, key, secret, algorithm, rsa_public_key, \
             tags, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.consumer_id)
        .bind(&input.key)
        .bind(&sealed_secret)
        .bind(input.algorithm.clone().unwrap_or_else(|| "HS256".into()))
        .bind(&input.rsa_public_key)
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;
        self.get_jwt(&id)
            .await?
            .ok_or_else(|| Error::Storage("credential disappeared after insert".into()))
    }

    pub async fn get_jwt(&self, id: &str) -> Result<Option<JwtSecret>> {
        let row = sqlx::query(&format!(
            "SELECT {JWT_COLUMNS} FROM jwt_secrets WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;
        row.as_ref()
            .map(|row| map_jwt(row, &self.secrets))
            .transpose()
    }

    pub async fn update_jwt(&self, id: &str, input: &UpdateJwtSecret) -> Result<JwtSecret> {
        let sealed_secret = input
            .secret
            .as_deref()
            .map(|value| self.seal_secret(value, "jwt_secrets", id, "secret"))
            .transpose()?;
        let rsa_set = input.rsa_public_key.is_some();
        let result = sqlx::query(
            "UPDATE jwt_secrets SET \
                key = COALESCE(?, key), \
                secret = COALESCE(?, secret), \
                algorithm = COALESCE(?, algorithm), \
                rsa_public_key = CASE WHEN ? THEN ? ELSE rsa_public_key END, \
                tags = COALESCE(?, tags) \
             WHERE id = ?",
        )
        .bind(&input.key)
        .bind(&sealed_secret)
        .bind(&input.algorithm)
        .bind(rsa_set)
        .bind(input.rsa_public_key.clone().flatten())
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "credential"))?;
        affected(result.rows_affected(), "credential")?;
        self.get_jwt(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: credential not found".into()))
    }

    pub async fn delete_jwt(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM jwt_secrets WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "credential"))?;
        affected(result.rows_affected(), "credential")
    }
}
