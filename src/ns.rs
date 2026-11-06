pub const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
pub const RDFS: &str = "http://www.w3.org/2000/01/rdf-schema#";
pub const OWL: &str = "http://www.w3.org/2002/07/owl#";
pub const SKOS: &str = "http://www.w3.org/2004/02/skos/core#";
pub const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
pub const QUDT: &str = "http://qudt.org/schema/qudt/";
pub const DCTERMS: &str = "http://purl.org/dc/terms/";

pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
pub const RDFS_COMMENT: &str = "http://www.w3.org/2000/01/rdf-schema#comment";
pub const RDFS_SUBCLASS_OF: &str = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
pub const RDFS_DOMAIN: &str = "http://www.w3.org/2000/01/rdf-schema#domain";
pub const RDFS_RANGE: &str = "http://www.w3.org/2000/01/rdf-schema#range";
pub const OWL_CLASS: &str = "http://www.w3.org/2002/07/owl#Class";
pub const OWL_OBJECT_PROPERTY: &str = "http://www.w3.org/2002/07/owl#ObjectProperty";
pub const OWL_SAME_AS: &str = "http://www.w3.org/2002/07/owl#sameAs";
pub const SKOS_PREF_LABEL: &str = "http://www.w3.org/2004/02/skos/core#prefLabel";
pub const SKOS_ALT_LABEL: &str = "http://www.w3.org/2004/02/skos/core#altLabel";
pub const XSD_DOUBLE: &str = "http://www.w3.org/2001/XMLSchema#double";
pub const DCTERMS_REFERENCES: &str = "http://purl.org/dc/terms/references";
pub const DCTERMS_REPLACES: &str = "http://purl.org/dc/terms/replaces";

pub const STANDARD_PREFIXES: [(&str, &str); 4] =
    [("rdf", RDF), ("rdfs", RDFS), ("owl", OWL), ("skos", SKOS)];

pub fn local_name(iri: &str) -> &str {
    iri.rsplit(['#', '/', ':']).next().unwrap_or(iri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_iris_are_in_their_namespace() {
        for (full, base, local) in [
            (RDF_TYPE, RDF, "type"),
            (RDFS_COMMENT, RDFS, "comment"),
            (RDFS_SUBCLASS_OF, RDFS, "subClassOf"),
            (RDFS_DOMAIN, RDFS, "domain"),
            (RDFS_RANGE, RDFS, "range"),
            (OWL_CLASS, OWL, "Class"),
            (OWL_OBJECT_PROPERTY, OWL, "ObjectProperty"),
            (OWL_SAME_AS, OWL, "sameAs"),
            (SKOS_PREF_LABEL, SKOS, "prefLabel"),
            (SKOS_ALT_LABEL, SKOS, "altLabel"),
            (XSD_DOUBLE, XSD, "double"),
            (DCTERMS_REFERENCES, DCTERMS, "references"),
            (DCTERMS_REPLACES, DCTERMS, "replaces"),
        ] {
            assert_eq!(full, format!("{base}{local}"));
        }
    }

    #[test]
    fn local_name_is_the_last_part() {
        assert_eq!(local_name("http://example.org/onto#Pump"), "Pump");
        assert_eq!(local_name("http://example.org/onto/Pump"), "Pump");
        assert_eq!(local_name("urn:x:Pump"), "Pump");
    }
}
