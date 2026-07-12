class Chunkloader < Formula
  desc "Dump webpack / Next.js / Framer / Flutter JS chunks and assets for analysis"
  homepage "https://github.com/Sn0wAlice/chunkloader"
  version "1.0.2"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Sn0wAlice/chunkloader/releases/download/v#{version}/chunkloader-darwin-arm64.tar.gz"
      sha256 "9712e4c7a8d12a37188e7d9456dba48b6582a4385619f1a76d44ef321cb2d3ff"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/Sn0wAlice/chunkloader/releases/download/v#{version}/chunkloader-linux-amd64.tar.gz"
      sha256 "9290bdd151de7239ddc4cce1c6ebb0fbb127acf6061da4caf68eeddd51bbf196"
    elsif Hardware::CPU.arm?
      url "https://github.com/Sn0wAlice/chunkloader/releases/download/v#{version}/chunkloader-linux-arm64.tar.gz"
      sha256 "170fa42627c74100e23f8c1938a06d4306252f784f8cb98448b6a6442836ab6b"
    end
  end

  def install
    bin.install "chunkloader"
  end

  test do
    assert_match "Usage", shell_output("#{bin}/chunkloader --help")
  end
end
