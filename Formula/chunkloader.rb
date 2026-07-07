class Chunkloader < Formula
  desc "Dump webpack / Next.js / Framer / Flutter JS chunks and assets for analysis"
  homepage "https://github.com/Sn0wAlice/chunkloader"
  version "0.1.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Sn0wAlice/chunkloader/releases/download/v#{version}/chunkloader-darwin-arm64.tar.gz"
      sha256 "ad3bf022311f7fb23f205db516aa23cb1abe1d1bcc26e7e87517945a89e569e7"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/Sn0wAlice/chunkloader/releases/download/v#{version}/chunkloader-linux-amd64.tar.gz"
      sha256 "7a84e3b3e97cd722dc285828ffcc596a03664b7960984260a27dce3b5db67d60"
    elsif Hardware::CPU.arm?
      url "https://github.com/Sn0wAlice/chunkloader/releases/download/v#{version}/chunkloader-linux-arm64.tar.gz"
      sha256 "e5c81e78957748d74206756242a9e5d06ddbc805f5f43cb84bcb1e419546b053"
    end
  end

  def install
    bin.install "chunkloader"
  end

  test do
    assert_match "Usage", shell_output("#{bin}/chunkloader --help")
  end
end
