SDK_URL := https://github.com/cmf-tech-community/cmf-watch-sdk.git
SDK_REV := cccaf794a372f98ec223ffa6f25db0960cb1c1c6
SDK := .sdk/wp2-apps

.PHONY: prepare build clean

$(SDK):
	mkdir -p .sdk
	git clone --quiet $(SDK_URL) $(SDK)
	git -C $(SDK) checkout --quiet $(SDK_REV)

prepare: $(SDK)
	rm -rf $(SDK)/sdk/examples/jusbrowse $(SDK)/sdk/companion-rpc
	cp -r jusbrowse $(SDK)/sdk/examples/jusbrowse
	cp -r companion-rpc $(SDK)/sdk/companion-rpc
	python3 -c 'p="$(SDK)/sdk/Cargo.toml"; s=open(p).read(); open(p,"w").write(s.replace("members = [", "members = [\"companion-rpc\", \"examples/jusbrowse\", ", 1) if "\"companion-rpc\"" not in s else s)'

build: prepare
	cd $(SDK) && scripts/build-app jusbrowse 1
	cp $(SDK)/target/jusbrowse.wp2app ./jusbrowse.wp2app

clean:
	rm -rf .sdk jusbrowse.wp2app
