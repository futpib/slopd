plugins {
    id("com.android.application")
    // The Flutter Gradle Plugin must be applied after the Android and Kotlin Gradle plugins.
    id("dev.flutter.flutter-gradle-plugin")
}

android {
    namespace = "io.github.futpib.slopd_control"
    compileSdk = flutter.compileSdkVersion
    ndkVersion = "28.2.13676358"

    buildFeatures {
        aidl = true
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    defaultConfig {
        applicationId = "io.github.futpib.slopd_control"
        minSdk = flutter.minSdkVersion
        if (project.findProperty("split-per-abi") != "true") {
            ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
        }
        targetSdk = flutter.targetSdkVersion
        versionCode = flutter.versionCode
        versionName = flutter.versionName
    }

    signingConfigs {
        create("release") {
            storeFile = System.getenv("KEYSTORE_PATH")?.takeIf { it.isNotBlank() }?.let { file(it) }
            storePassword = System.getenv("KEYSTORE_PASSWORD")
            keyAlias = System.getenv("KEY_ALIAS")?.takeIf { it.isNotBlank() } ?: "release"
            keyPassword = System.getenv("KEY_PASSWORD")?.takeIf { it.isNotBlank() } ?: storePassword
        }
    }

    buildTypes {
        release {
            signingConfig = signingConfigs.getByName("release")
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17
    }
}

flutter {
    source = "../.."
}

val validateReleaseCredentials by tasks.registering {
    doLast {
        val keystorePath = System.getenv("KEYSTORE_PATH")
        require(!keystorePath.isNullOrBlank()) { "Set KEYSTORE_PATH for release signing" }
        require(file(keystorePath).isFile) { "KEYSTORE_PATH must point to a release keystore" }
        require(!System.getenv("KEYSTORE_PASSWORD").isNullOrBlank()) {
            "Set KEYSTORE_PASSWORD for release signing"
        }
    }
}
tasks.matching { it.name == "preReleaseBuild" }.configureEach {
    dependsOn(validateReleaseCredentials)
}

val buildIrohTransport by tasks.registering(Exec::class) {
    mustRunAfter(validateReleaseCredentials)
    workingDir = file("../..")
    commandLine("bash", "scripts/build-native.sh")
}
tasks.named("preBuild") { dependsOn(buildIrohTransport) }

repositories {
    maven { url = uri("https://github.com/rustls/rustls-platform-verifier/raw/maven-archive/android-release-support/maven/") }
}

// Keep the JVM verifier matched to the Rust dependency in the committed lockfile.
val verifierLock = file("../../native/Cargo.lock").readText()
val verifierVersion = Regex("name = \"rustls-platform-verifier-android\"\\nversion = \"([^\"]+)\"")
    .find(verifierLock)?.groupValues?.get(1) ?: error("Missing Rust Android verifier dependency")
dependencies { implementation("org.rustls:rustls-platform-verifier:$verifierVersion") }
