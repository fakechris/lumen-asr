#!/usr/bin/env swift
/// Create a *trusted* self-signed code-signing identity in the login keychain.
/// No Apple Developer Program required. Identity is stable across rebuilds so TCC
/// (Microphone / Accessibility) can stick to the same certificate requirement.
///
/// Usage:
///   swift scripts/macos/create_local_codesign_identity.swift
///   swift scripts/macos/create_local_codesign_identity.swift "Lumen Local Codesign"

import Foundation
import Security

let commonName = CommandLine.arguments.count > 1
  ? CommandLine.arguments[1]
  : "Lumen Local Codesign"

func die(_ msg: String) -> Never {
  fputs("ERROR: \(msg)\n", stderr)
  exit(1)
}

// Already have a *valid* codesigning identity with this label?
func hasValidIdentity(named name: String) -> Bool {
  let query: [String: Any] = [
    kSecClass as String: kSecClassIdentity,
    kSecMatchLimit as String: kSecMatchLimitAll,
    kSecReturnRef as String: true,
    kSecReturnAttributes as String: true,
  ]
  var raw: CFTypeRef?
  let status = SecItemCopyMatching(query as CFDictionary, &raw)
  guard status == errSecSuccess, let items = raw as? [[String: Any]] else {
    return false
  }
  for item in items {
    // Label / labl
    let label =
      (item[kSecAttrLabel as String] as? String)
      ?? (item["labl"] as? String)
      ?? ""
    if label != name { continue }
    if let idRef = item[kSecValueRef as String] {
      let identity = idRef as! SecIdentity
      var cert: SecCertificate?
      if SecIdentityCopyCertificate(identity, &cert) == errSecSuccess, let cert {
        // Prefer identities codesign will accept: check trust for code signing.
        var trust: SecTrust?
        let policy = SecPolicyCreateCodeSigning()
        if SecTrustCreateWithCertificates(cert, policy, &trust) == errSecSuccess,
          let trust
        {
          var error: CFError?
          if SecTrustEvaluateWithError(trust, &error) {
            return true
          }
        }
      }
    }
  }
  return false
}

if hasValidIdentity(named: commonName) {
  print("OK: valid codesigning identity already present: \(commonName)")
  exit(0)
}

// --- Generate RSA private key in keychain ---
let keyTag = "com.lumenasr.local-codesign.\(commonName)".data(using: .utf8)!

// Delete stale key/cert with same tag if any (best-effort).
let delKey: [String: Any] = [
  kSecClass as String: kSecClassKey,
  kSecAttrApplicationTag as String: keyTag,
]
SecItemDelete(delKey as CFDictionary)

let keyParams: [String: Any] = [
  kSecAttrKeyType as String: kSecAttrKeyTypeRSA,
  kSecAttrKeySizeInBits as String: 2048,
  kSecAttrIsPermanent as String: true,
  kSecAttrApplicationTag as String: keyTag,
  kSecAttrLabel as String: commonName,
  kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlock,
]
var error: Unmanaged<CFError>?
guard let privateKey = SecKeyCreateRandomKey(keyParams as CFDictionary, &error) else {
  die("SecKeyCreateRandomKey: \(error!.takeRetainedValue())")
}
guard let publicKey = SecKeyCopyPublicKey(privateKey) else {
  die("SecKeyCopyPublicKey failed")
}

// --- Build a minimal self-signed certificate via SecItem (no openssl) ---
// Use openssl only to assemble the X.509 blob from the keypair would reintroduce trust issues.
// Instead: use Security's certificate creation if available (macOS 10.3+ via SecCertificateCreate).
//
// Practical approach on modern macOS: export public key, build CSR with openssl CLI is messy.
// We use the fact that `SecItemAdd` of a PKCS12 produced with Apple-compatible algorithms works
// when combined with `SecTrustSettingsSetTrustSettings` for code signing.

// Export private key as PKCS#1 so we can re-import via a tightly controlled openssl -legacy p12,
// then set trust with SecTrustSettingsSetTrustSettings (user domain, no admin if possible).

var exportErr: Unmanaged<CFError>?
guard let privData = SecKeyCopyExternalRepresentation(privateKey, &exportErr) as Data? else {
  die("export private key: \(exportErr!.takeRetainedValue())")
}

let work = FileManager.default.temporaryDirectory.appendingPathComponent("lumen-cs-\(UUID().uuidString)")
try! FileManager.default.createDirectory(at: work, withIntermediateDirectories: true)

// PKCS#1 RSA private key needs ASN.1 wrapping for openssl. SecKey external rep for RSA is PKCS#1.
let privPemPath = work.appendingPathComponent("key.pem")
let b64 = privData.base64EncodedString(options: [.lineLength64Characters, .endLineWithLineFeed])
let pem = "-----BEGIN RSA PRIVATE KEY-----\n\(b64)\n-----END RSA PRIVATE KEY-----\n"
try! pem.write(to: privPemPath, atomically: true, encoding: .utf8)

let cnfPath = work.appendingPathComponent("openssl.cnf")
let cnf = """
[req]
distinguished_name = req_distinguished_name
x509_extensions = v3_codesign
prompt = no
[req_distinguished_name]
CN = \(commonName)
O = Lumen Local Dev
C = US
[v3_codesign]
basicConstraints = CA:TRUE
keyUsage = critical, digitalSignature, keyCertSign
extendedKeyUsage = critical, codeSigning
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid:always,issuer
"""
try! cnf.write(to: cnfPath, atomically: true, encoding: .utf8)

let certPath = work.appendingPathComponent("cert.pem")
let p12Path = work.appendingPathComponent("identity.p12")
let pass = "lumen-local-dev"

func run(_ args: [String]) {
  let p = Process()
  p.executableURL = URL(fileURLWithPath: "/usr/bin/env")
  p.arguments = args
  let err = Pipe()
  p.standardError = err
  p.standardOutput = Pipe()
  try! p.run()
  p.waitUntilExit()
  if p.terminationStatus != 0 {
    let msg = String(data: err.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
    die("command failed (\(args.joined(separator: " "))): \(msg)")
  }
}

// Prefer Homebrew openssl for -legacy; fall back to openssl in PATH.
let openssl: String = {
  let brew = "/opt/homebrew/bin/openssl"
  if FileManager.default.isExecutableFile(atPath: brew) { return brew }
  return "openssl"
}()

run([
  openssl, "req", "-new", "-x509", "-days", "3650",
  "-key", privPemPath.path,
  "-out", certPath.path,
  "-config", cnfPath.path,
  "-extensions", "v3_codesign",
])

// PKCS#12 macOS-compatible (OpenSSL 3 needs -legacy)
var p12Args = [
  openssl, "pkcs12", "-export",
  "-out", p12Path.path,
  "-inkey", privPemPath.path,
  "-in", certPath.path,
  "-name", commonName,
  "-passout", "pass:\(pass)",
]
// Detect -legacy support
let help = Process()
help.executableURL = URL(fileURLWithPath: "/usr/bin/env")
help.arguments = [openssl, "pkcs12", "-export", "-help"]
let helpPipe = Pipe()
help.standardError = helpPipe
help.standardOutput = helpPipe
try! help.run()
help.waitUntilExit()
let helpOut = String(data: helpPipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
if helpOut.contains("-legacy") {
  p12Args.insert("-legacy", at: 3)
} else {
  p12Args += ["-certpbe", "PBE-SHA1-3DES", "-keypbe", "PBE-SHA1-3DES", "-macalg", "SHA1"]
}
run(p12Args)

// Import PKCS#12 into login keychain
let p12Data = try! Data(contentsOf: p12Path)
var importErr: Unmanaged<CFError>?
// Remove previous cert with same label (best effort) — user may re-run.
let options: [String: Any] = [
  kSecImportExportPassphrase as String: pass,
]
var items: CFArray?
let impStatus = SecPKCS12Import(p12Data as CFData, options as CFDictionary, &items)
if impStatus != errSecSuccess {
  die("SecPKCS12Import failed: \(impStatus)")
}

guard let arr = items as? [[String: Any]], let first = arr.first else {
  die("SecPKCS12Import returned no items")
}
guard let identity = first[kSecImportItemIdentity as String] as! SecIdentity? else {
  die("no identity in p12")
}
var cert: SecCertificate?
SecIdentityCopyCertificate(identity, &cert)
guard let cert else { die("no certificate on identity") }

// Store identity permanently (import already added items; ensure label)
let addIdentity: [String: Any] = [
  kSecClass as String: kSecClassIdentity,
  kSecValueRef as String: identity,
  kSecAttrLabel as String: commonName,
]
// May return duplicate — ignore.
let addSt = SecItemAdd(addIdentity as CFDictionary, nil)
if addSt != errSecSuccess && addSt != errSecDuplicateItem {
  // PKCS12 import already placed it; continue.
  fputs("note: SecItemAdd identity status \(addSt)\n", stderr)
}

// Trust this certificate for code signing in *user* domain (no sudo).
// On recent macOS this often requires user confirmation UI once.
var trustSettings: [[String: Any]] = [[
  kSecTrustSettingsResult as String: NSNumber(value: SecTrustSettingsResult.trustRoot.rawValue),
  kSecTrustSettingsPolicy as String: SecPolicyCreateCodeSigning(),
]]
// Also allow as root for SSL-less local codesign evaluation paths.
trustSettings.append([
  kSecTrustSettingsResult as String: NSNumber(value: SecTrustSettingsResult.trustRoot.rawValue),
])

let trustStatus = SecTrustSettingsSetTrustSettings(
  cert,
  .user,
  trustSettings as CFTypeRef
)
if trustStatus != errSecSuccess {
  fputs(
    """
    WARN: SecTrustSettingsSetTrustSettings status \(trustStatus)
    If codesign still says NOT_TRUSTED, do this once (GUI):
      1. Open Keychain Access
      2. Find certificate "\(commonName)" under login → Certificates
      3. Double-click → Trust → Code Signing = \"Always Trust\"
      4. Close and enter login keychain password if prompted
    """,
    stderr
  )
} else {
  print("Trust settings applied for code signing (user domain).")
}

// Allow codesign to use the key without interactive prompt (best-effort).
// Partition list still may require keychain unlock.
print("Created identity: \(commonName)")
print("Verify with: security find-identity -v -p codesigning | grep '\(commonName)'")

// Cleanup temp
try? FileManager.default.removeItem(at: work)

if hasValidIdentity(named: commonName) {
  print("OK: identity is valid for codesigning.")
  exit(0)
} else {
  fputs(
    """
    Identity imported but not yet evaluated as trusted.
    Complete the Keychain Access trust step above, then re-run:
      security find-identity -v -p codesigning
    """,
    stderr
  )
  exit(2)
}
