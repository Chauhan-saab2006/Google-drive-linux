# Google Drive Linux — Google OAuth 2.0 Specification & Guide

## 1. Authentication Flow Overview

Google Drive Linux employs the standard OAuth 2.0 authorization code flow with **PKCE (Proof Key for Code Exchange - RFC 7636)** and a local loopback listener:

```
+------------------+         +------------------+         +--------------------+
|  google-drive-   |         |  Default Web     |         |  Google Accounts   |
|  linux Client    |         |  Browser         |         |  OAuth 2.0 Server  |
+--------+---------+         +--------+---------+         +---------+----------+
         |                            |                             |
         | 1. Bind 127.0.0.1:<port>   |                             |
         | 2. Generate PKCE S256 pair |                             |
         | 3. Launch browser url ---->|                             |
         |                            | 4. User signs in & consents |
         |                            |---------------------------->|
         |                            |                             |
         |                            | 5. Redirect callback        |
         | 6. HTTP GET /callback <----|<----------------------------|
         |    with code & state       |                             |
         | 7. Return 200 OK HTML ---->|                             |
         |                            |                             |
         | 8. POST /token (code + code_verifier)                    |
         |--------------------------------------------------------->|
         | 9. Returns access_token, refresh_token, expires_in       |
         |<---------------------------------------------------------|
         |                                                          |
         | 10. GET /userinfo (email & name)                         |
         |--------------------------------------------------------->|
         | 11. Save to ~/.local/share/google-drive-linux/credentials.json (0600)
         |                                                          |
```

---

## 2. Obtaining a Google Cloud OAuth 2.0 Client ID

To use Google Drive Linux with your own Google account, you can create a free OAuth 2.0 Client ID in the Google Cloud Console:

1. Go to the [Google Cloud Console](https://console.cloud.google.com/).
2. Create a new project (e.g. `Google Drive Linux`).
3. Under **APIs & Services** > **Enabled APIs & Services**, click **+ Enable APIs and Services** and enable the **Google Drive API**.
4. Go to **APIs & Services** > **OAuth consent screen**:
   - Select User Type: **External**.
   - Fill in App Name (e.g. `Google Drive Linux`) and your contact email.
   - Under Scopes, add:
     - `.../auth/drive`
     - `.../auth/userinfo.email`
     - `.../auth/userinfo.profile`
   - Under Test users, add your Google email address.
5. Go to **APIs & Services** > **Credentials**:
   - Click **Create Credentials** > **OAuth client ID**.
   - Application type: **Desktop app**.
   - Name: `Google Drive Linux Desktop`.
   - Click **Create**.
6. Copy your **Client ID** and **Client Secret**.

---

## 3. Configuring Your Credentials

You can supply your Client ID in three convenient ways:

### Option A: Command Line Argument
```bash
google-drive-linux login --client-id "YOUR_CLIENT_ID.apps.googleusercontent.com" --client-secret "YOUR_SECRET"
```

### Option B: Configuration File (`~/.config/google-drive-linux/config.toml`)
```toml
client_id = "YOUR_CLIENT_ID.apps.googleusercontent.com"
client_secret = "YOUR_SECRET"
```

### Option C: Environment Variables
```bash
export GOOGLE_CLIENT_ID="YOUR_CLIENT_ID.apps.googleusercontent.com"
export GOOGLE_CLIENT_SECRET="YOUR_SECRET"
google-drive-linux login
```

---

## 4. Token Storage Security

Credentials are saved in:
```
$XDG_DATA_HOME/google-drive-linux/credentials.json
(typically ~/.local/share/google-drive-linux/credentials.json)
```

### Security Measures:
- **File Permissions**: Strictly restricted to Unix mode `0600` (read and write only by the owner).
- **Directory Permissions**: Parent directory is enforced to Unix mode `0700`.
- **No Password Storage**: Google passwords are never requested, handled, or stored.
- **PKCE Protection**: Prevents authorization code interception attacks.
- **CSRF State Validation**: Random 16-byte state parameter is checked on callback.

---

## 5. Token Refresh Mechanism

Access tokens expire after 1 hour (3600 seconds).
The application transparently checks `expires_at` before making any API request:
- If `now + 300 >= expires_at` (within 5 minutes of expiring), it uses the refresh token to request a new access token from `https://oauth2.googleapis.com/token`.
- Stored credentials are automatically updated with the new token and expiration timestamp.

---

## 6. Remote Revocation & Logout

When logging out:
```bash
google-drive-linux logout
```
1. The client issues an HTTP POST request to Google's revocation endpoint:
   ```
   https://oauth2.googleapis.com/revoke?token=<refresh_token>
   ```
2. The local `credentials.json` file is deleted from disk.
3. The application status changes to `Disconnected`.
