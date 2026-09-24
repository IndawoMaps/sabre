import ExpoModulesCore
import SabreFFI

final class SabreStartException: GenericException<String> {
  override var reason: String { param }
}

public class SabreModule: Module {
  /// The app's Documents directory: the default file root.
  private var defaultRoot: String {
    FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0].path
  }

  public func definition() -> ModuleDefinition {
    Name("Sabre")

    AsyncFunction("start") { (fileRoot: String?, cacheBytes: Double) throws -> [String: Any] in
      let root = fileRoot ?? self.defaultRoot
      let ptr = sabre_start(root, UInt64(cacheBytes))
      defer { sabre_string_free(ptr) }
      let text = ptr.map { String(cString: $0) } ?? "{\"error\":\"sabre_start returned null\"}"
      let json = (try? JSONSerialization.jsonObject(with: Data(text.utf8))) as? [String: Any] ?? [:]
      if let error = json["error"] as? String {
        throw SabreStartException(error)
      }
      return [
        "port": json["port"] ?? 0,
        "token": json["token"] ?? "",
        "baseUrl": json["base_url"] ?? "",
        "fileRoot": root,
      ]
    }

    Function("stop") {
      sabre_stop()
    }

    Function("listRasters") { (fileRoot: String?) -> [String] in
      let names = (try? FileManager.default.contentsOfDirectory(atPath: fileRoot ?? self.defaultRoot)) ?? []
      return names.filter { $0.hasSuffix(".tif") || $0.hasSuffix(".tiff") }.sorted()
    }
  }
}
