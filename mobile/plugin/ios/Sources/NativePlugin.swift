import Foundation
import SwiftRs
import Tauri
import UIKit
import WebKit

struct PathArgs: Decodable {
  let path: String
}

struct KeepRunningArgs: Decodable {
  let on: Bool
}

class NativePlugin: Plugin, UIDocumentInteractionControllerDelegate {
  // Kept alive while its preview is on screen.
  private var doc: UIDocumentInteractionController?

  // The page lays itself out edge to edge with CSS env(safe-area-inset-*). Left on
  // .automatic, the scroll view also insets by the safe area, so the page ended
  // above the home indicator and the strip below it showed the white native view.
  override func load(webview: WKWebView) {
    webview.scrollView.contentInsetAdjustmentBehavior = .never
  }

  private func fileURL(_ invoke: Invoke) -> URL? {
    guard let args = try? invoke.parseArgs(PathArgs.self) else {
      invoke.reject("잘못된 요청")
      return nil
    }
    let url = URL(fileURLWithPath: args.path)
    if !FileManager.default.fileExists(atPath: url.path) {
      invoke.reject("파일이 없습니다")
      return nil
    }
    return url
  }

  /// Anchors popovers on iPad, where a sheet without an anchor crashes.
  private func anchor(_ vc: UIViewController, in host: UIViewController) {
    if let pop = vc.popoverPresentationController {
      pop.sourceView = host.view
      pop.sourceRect = CGRect(x: host.view.bounds.midX, y: host.view.bounds.midY, width: 0, height: 0)
      pop.permittedArrowDirections = []
    }
  }

  @objc public func open(_ invoke: Invoke) throws {
    guard let url = fileURL(invoke) else { return }
    DispatchQueue.main.async {
      guard let host = self.manager.viewController else {
        invoke.reject("화면을 찾을 수 없습니다")
        return
      }
      let d = UIDocumentInteractionController(url: url)
      d.delegate = self
      self.doc = d
      // Quick Look first; files it cannot show get the "open in" menu.
      if !d.presentPreview(animated: true) {
        if !d.presentOptionsMenu(from: host.view.bounds, in: host.view, animated: true) {
          invoke.reject("이 파일을 열 수 있는 앱이 없습니다. 공유를 사용하세요")
          return
        }
      }
      invoke.resolve()
    }
  }

  func documentInteractionControllerViewControllerForPreview(_ controller: UIDocumentInteractionController) -> UIViewController {
    return manager.viewController ?? UIViewController()
  }

  func documentInteractionControllerDidEndPreview(_ controller: UIDocumentInteractionController) {
    doc = nil
  }

  @objc public func share(_ invoke: Invoke) throws {
    guard let url = fileURL(invoke) else { return }
    DispatchQueue.main.async {
      guard let host = self.manager.viewController else {
        invoke.reject("화면을 찾을 수 없습니다")
        return
      }
      let sheet = UIActivityViewController(activityItems: [url], applicationActivities: nil)
      self.anchor(sheet, in: host)
      host.present(sheet, animated: true)
      invoke.resolve()
    }
  }

  /// "Save to Files": the user picks the folder.
  @objc public func save(_ invoke: Invoke) throws {
    guard let url = fileURL(invoke) else { return }
    DispatchQueue.main.async {
      guard let host = self.manager.viewController else {
        invoke.reject("화면을 찾을 수 없습니다")
        return
      }
      let picker = UIDocumentPickerViewController(forExporting: [url], asCopy: true)
      self.anchor(picker, in: host)
      host.present(picker, animated: true)
      invoke.resolve(["saved": ""])
    }
  }

  // Background time held while the relay connection signs off (see keepRunning).
  private var task: UIBackgroundTaskIdentifier = .invalid

  /// iOS freezes an app as soon as it leaves the screen, which would leave the relay
  /// connection open and the device looking online. The app asks for a few seconds
  /// of background time while connected and gives it back once it has logged off.
  @objc public func keepRunning(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(KeepRunningArgs.self)
    DispatchQueue.main.async {
      if args.on {
        if self.task == .invalid {
          self.task = UIApplication.shared.beginBackgroundTask(withName: "relay-logout") { self.endTask() }
        }
      } else {
        self.endTask()
      }
      invoke.resolve()
    }
  }

  private func endTask() {
    if task != .invalid {
      UIApplication.shared.endBackgroundTask(task)
      task = .invalid
    }
  }

  @objc public func haptic(_ invoke: Invoke) throws {
    DispatchQueue.main.async {
      UINotificationFeedbackGenerator().notificationOccurred(.success)
      invoke.resolve()
    }
  }
}

@_cdecl("init_plugin_pm_native")
func initPlugin() -> Plugin {
  return NativePlugin()
}
