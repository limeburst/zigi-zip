use minijinja::{context, Environment, Value};
use once_cell::sync::Lazy;

static ENV: Lazy<Environment<'static>> = Lazy::new(|| {
    let mut env = Environment::new();
    env.add_template("layout.html", include_str!("../templates/layout.html")).unwrap();
    env.add_template("index.html", include_str!("../templates/index.html")).unwrap();
    env.add_template("upload_form.html", include_str!("../templates/upload_form.html")).unwrap();
    env.add_template("upload_success.html", include_str!("../templates/upload_success.html")).unwrap();
    env.add_template("paste_form.html", include_str!("../templates/paste_form.html")).unwrap();
    env.add_template("editor.html", include_str!("../templates/editor.html")).unwrap();
    env.add_template("admin_login.html", include_str!("../templates/admin_login.html")).unwrap();
    env.add_template("admin_view.html", include_str!("../templates/admin_view.html")).unwrap();
    env
});

const BASE_CSS: &str = include_str!("../templates/base.css");

pub fn render(name: &str, ctx: Value) -> String {
    let tmpl = ENV.get_template(name).expect("template missing");
    tmpl.render(context! { base_css => BASE_CSS, ..ctx }).expect("render")
}
