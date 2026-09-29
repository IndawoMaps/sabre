package com.sabremaps.mobile

import expo.modules.kotlin.exception.CodedException
import expo.modules.kotlin.modules.Module
import expo.modules.kotlin.modules.ModuleDefinition
import java.io.File
import org.json.JSONObject

/** The Rust side, crates/mobile/src/android.rs. */
object SabreNative {
  init {
    System.loadLibrary("sabre_mobile")
  }

  @JvmStatic external fun start(fileRoot: String, cacheBytes: Long): String
  @JvmStatic external fun stop()
}

class SabreStartException(message: String) : CodedException(message)

class SabreModule : Module() {
  /** The app's private files directory: the default, and only sensible, file root. */
  private val defaultRoot: String
    get() = appContext.reactContext?.filesDir?.absolutePath
      ?: throw SabreStartException("no Android context to find the files directory in")

  override fun definition() = ModuleDefinition {
    Name("Sabre")

    AsyncFunction("start") { fileRoot: String?, cacheBytes: Double ->
      val json = JSONObject(SabreNative.start(fileRoot ?: defaultRoot, cacheBytes.toLong()))
      if (json.has("error")) {
        throw SabreStartException(json.getString("error"))
      }
      mapOf(
        "port" to json.getInt("port"),
        "token" to json.getString("token"),
        "baseUrl" to json.getString("base_url"),
        "fileRoot" to (fileRoot ?: defaultRoot),
      )
    }

    Function("stop") {
      SabreNative.stop()
    }

    /** Names of the GeoTIFFs directly inside `fileRoot`, for a picker. */
    Function("listRasters") { fileRoot: String? ->
      File(fileRoot ?: defaultRoot).listFiles().orEmpty()
        .filter { it.isFile && (it.name.endsWith(".tif", ignoreCase = true) || it.name.endsWith(".tiff", ignoreCase = true)) }
        .map { it.name }
        .sorted()
    }
  }
}
